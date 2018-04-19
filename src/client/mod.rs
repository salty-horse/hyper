//! HTTP Client

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures::{Async, Future, Poll};
use futures::future::{self, Either, Executor};
use futures::sync::oneshot;
use http::{Method, Request, Response, Uri, Version};
use http::header::{Entry, HeaderValue, HOST};
use http::uri::Scheme;

use body::{Body, Payload};
use common::Exec;
use self::pool::{Pool, Poolable, Reservation};

pub use self::connect::Connect;
#[cfg(feature = "runtime")] pub use self::connect::HttpConnector;

use self::connect::Destination;

pub mod conn;
pub mod connect;
pub(crate) mod dispatch;
#[cfg(feature = "runtime")] mod dns;
mod pool;
#[cfg(test)]
mod tests;

/// A Client to make outgoing HTTP requests.
pub struct Client<C, B = Body> {
    connector: Arc<C>,
    executor: Exec,
    h1_writev: bool,
    pool: Pool<PoolClient<B>>,
    retry_canceled_requests: bool,
    set_host: bool,
    ver: Ver,
}

#[cfg(feature = "runtime")]
impl Client<HttpConnector, Body> {
    /// Create a new Client with the default config.
    #[inline]
    pub fn new() -> Client<HttpConnector, Body> {
        Builder::default().build_http()
    }
}

#[cfg(feature = "runtime")]
impl Default for Client<HttpConnector, Body> {
    fn default() -> Client<HttpConnector, Body> {
        Client::new()
    }
}

impl Client<(), Body> {
    /// Configure a Client.
    ///
    /// # Example
    ///
    /// ```
    /// use hyper::Client;
    ///
    /// let client = Client::builder()
    ///     .keep_alive(true)
    ///     .build_http();
    /// # let infer: Client<_, hyper::Body> = client;
    /// # drop(infer);
    /// ```
    #[inline]
    pub fn builder() -> Builder {
        Builder::default()
    }
}

impl<C, B> Client<C, B>
where C: Connect + Sync + 'static,
      C::Transport: 'static,
      C::Future: 'static,
      B: Payload + Send + 'static,
      B::Data: Send,
{

    /// Send a `GET` request to the supplied `Uri`.
    ///
    /// # Note
    ///
    /// This requires that the `Payload` type have a `Default` implementation.
    /// It *should* return an "empty" version of itself, such that
    /// `Payload::is_end_stream` is `true`.
    pub fn get(&self, uri: Uri) -> FutureResponse
    where
        B: Default,
    {
        let body = B::default();
        if !body.is_end_stream() {
            warn!("default Payload used for get() does not return true for is_end_stream");
        }

        let mut req = Request::new(body);
        *req.uri_mut() = uri;
        self.request(req)
    }

    /// Send a constructed Request using this Client.
    pub fn request(&self, mut req: Request<B>) -> FutureResponse {
        // TODO(0.12): do this at construction time.
        //
        // It cannot be done in the constructor because the Client::configured
        // does not have `B: 'static` bounds, which are required to spawn
        // the interval. In 0.12, add a static bounds to the constructor,
        // and move this.
        self.schedule_pool_timer();

        match req.version() {
            Version::HTTP_10 |
            Version::HTTP_11 => (),
            other => {
                error!("Request has unsupported version \"{:?}\"", other);
                //TODO: replace this with a proper variant
                return FutureResponse(Box::new(future::err(::Error::new_user_unsupported_version())));
            }
        }

        if req.method() == &Method::CONNECT {
            debug!("Client does not support CONNECT requests");
            return FutureResponse(Box::new(future::err(::Error::new_user_unsupported_request_method())));
        }

        let uri = req.uri().clone();
        let domain = match (uri.scheme_part(), uri.authority_part()) {
            (Some(scheme), Some(auth)) => {
                format!("{}://{}", scheme, auth)
            }
            _ => {
                //TODO: replace this with a proper variant
                return FutureResponse(Box::new(future::err(::Error::new_io(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid URI for Client Request"
                    )
                ))));
            }
        };

        if self.set_host && self.ver == Ver::Http1 {
            if let Entry::Vacant(entry) = req.headers_mut().entry(HOST).expect("HOST is always valid header name") {
                let hostname = uri.host().expect("authority implies host");
                let host = if let Some(port) = uri.port() {
                    let s = format!("{}:{}", hostname, port);
                    HeaderValue::from_str(&s)
                } else {
                    HeaderValue::from_str(hostname)
                }.expect("uri host is valid header value");
                entry.insert(host);
            }
        }


        let client = self.clone();
        let uri = req.uri().clone();
        let fut = RetryableSendRequest {
            client: client,
            future: self.send_request(req, &domain),
            domain: domain,
            uri: uri,
        };
        FutureResponse(Box::new(fut))
    }

    //TODO: replace with `impl Future` when stable
    fn send_request(&self, mut req: Request<B>, domain: &str) -> Box<Future<Item=Response<Body>, Error=ClientError<B>> + Send> {
        let url = req.uri().clone();
        let ver = self.ver;
        let pool_key = (Arc::new(domain.to_string()), self.ver);
        let checkout = self.pool.checkout(pool_key.clone());
        let connect = {
            let executor = self.executor.clone();
            let pool = self.pool.clone();
            let h1_writev = self.h1_writev;
            let connector = self.connector.clone();
            let dst = Destination {
                uri: url,
            };
            future::lazy(move || {
                if let Some(connecting) = pool.connecting(&pool_key) {
                    Either::A(connector.connect(dst)
                        .map_err(::Error::new_connect)
                        .and_then(move |(io, connected)| {
                            conn::Builder::new()
                                .h1_writev(h1_writev)
                                .http2_only(pool_key.1 == Ver::Http2)
                                .handshake_no_upgrades(io)
                                .and_then(move |(tx, conn)| {
                                    executor.execute(conn.map_err(|e| {
                                        debug!("client connection error: {}", e)
                                    }));

                                    // Wait for 'conn' to ready up before we
                                    // declare this tx as usable
                                    tx.when_ready()
                                })
                                .map(move |tx| {
                                    pool.pooled(connecting, PoolClient {
                                        is_proxied: connected.is_proxied,
                                        tx: match ver {
                                            Ver::Http1 => PoolTx::Http1(tx),
                                            Ver::Http2 => PoolTx::Http2(tx.into_http2()),
                                        },
                                    })
                                })
                        }))
                } else {
                    let canceled = ::Error::new_canceled(Some("HTTP/2 connection in progress"));
                    Either::B(future::err(canceled))
                }
            })
        };

        let race = checkout.select(connect)
            .map(|(pooled, _work)| pooled)
            .or_else(|(e, other)| {
                // Either checkout or connect could get canceled:
                //
                // 1. Connect is canceled if this is HTTP/2 and there is
                //    an outstanding HTTP/2 connecting task.
                // 2. Checkout is canceled if the pool cannot deliver an
                //    idle connection reliably.
                //
                // In both cases, we should just wait for the other future.
                if e.is_canceled() {
                    //trace!("checkout/connect race canceled: {}", e);
                    Either::A(other.map_err(ClientError::Normal))
                } else {
                    Either::B(future::err(ClientError::Normal(e)))
                }
            });

        let executor = self.executor.clone();
        let resp = race.and_then(move |mut pooled| {
            let conn_reused = pooled.is_reused();
            if ver == Ver::Http1 {
                set_relative_uri(req.uri_mut(), pooled.is_proxied);
            }
            let fut = pooled.send_request_retryable(req)
                .map_err(move |(err, orig_req)| {
                    if let Some(req) = orig_req {
                        ClientError::Canceled {
                            connection_reused: conn_reused,
                            reason: err,
                            req,
                        }
                    } else {
                        ClientError::Normal(err)
                    }
                })
                .and_then(move |mut res| {
                    // If pooled is HTTP/2, we can toss this reference immediately.
                    //
                    // when pooled is dropped, it will try to insert back into the
                    // pool. To delay that, spawn a future that completes once the
                    // sender is ready again.
                    //
                    // This *should* only be once the related `Connection` has polled
                    // for a new request to start.
                    //
                    // It won't be ready if there is a body to stream.
                    if ver == Ver::Http2 || pooled.is_ready() {
                        drop(pooled);
                    } else if !res.body().is_empty() {
                        let (delayed_tx, delayed_rx) = oneshot::channel();
                        res.body_mut().delayed_eof(delayed_rx);
                        executor.execute(
                            future::poll_fn(move || {
                                pooled.poll_ready()
                            })
                            .then(move |_| {
                                // At this point, `pooled` is dropped, and had a chance
                                // to insert into the pool (if conn was idle)
                                drop(delayed_tx);
                                Ok(())
                            })
                        );
                    }
                    Ok(res)
                });

            fut
        });

        Box::new(resp)
    }

    fn schedule_pool_timer(&self) {
        self.pool.spawn_expired_interval(&self.executor);
    }
}

impl<C, B> Clone for Client<C, B> {
    fn clone(&self) -> Client<C, B> {
        Client {
            connector: self.connector.clone(),
            executor: self.executor.clone(),
            h1_writev: self.h1_writev,
            pool: self.pool.clone(),
            retry_canceled_requests: self.retry_canceled_requests,
            set_host: self.set_host,
            ver: self.ver,
        }
    }
}

impl<C, B> fmt::Debug for Client<C, B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Client")
            .finish()
    }
}

/// A `Future` that will resolve to an HTTP Response.
#[must_use = "futures do nothing unless polled"]
pub struct FutureResponse(Box<Future<Item=Response<Body>, Error=::Error> + Send + 'static>);

impl fmt::Debug for FutureResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("Future<Response>")
    }
}

impl Future for FutureResponse {
    type Item = Response<Body>;
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

struct RetryableSendRequest<C, B> {
    client: Client<C, B>,
    domain: String,
    future: Box<Future<Item=Response<Body>, Error=ClientError<B>> + Send>,
    uri: Uri,
}

impl<C, B> Future for RetryableSendRequest<C, B>
where
    C: Connect + 'static,
    C::Future: 'static,
    B: Payload + Send + 'static,
    B::Data: Send,
{
    type Item = Response<Body>;
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.future.poll() {
                Ok(Async::Ready(resp)) => return Ok(Async::Ready(resp)),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(ClientError::Normal(err)) => return Err(err),
                Err(ClientError::Canceled {
                    connection_reused,
                    mut req,
                    reason,
                }) => {
                    if !self.client.retry_canceled_requests || !connection_reused {
                        // if client disabled, don't retry
                        // a fresh connection means we definitely can't retry
                        return Err(reason);
                    }

                    trace!("unstarted request canceled, trying again (reason={:?})", reason);
                    *req.uri_mut() = self.uri.clone();
                    self.future = self.client.send_request(req, &self.domain);
                }
            }
        }
    }
}

struct PoolClient<B> {
    is_proxied: bool,
    tx: PoolTx<B>,
}

enum PoolTx<B> {
    Http1(conn::SendRequest<B>),
    Http2(conn::Http2SendRequest<B>),
}

impl<B> PoolClient<B> {
    fn poll_ready(&mut self) -> Poll<(), ::Error> {
        match self.tx {
            PoolTx::Http1(ref mut tx) => tx.poll_ready(),
            PoolTx::Http2(_) => Ok(Async::Ready(())),
        }
    }

    fn is_ready(&self) -> bool {
        match self.tx {
            PoolTx::Http1(ref tx) => tx.is_ready(),
            PoolTx::Http2(ref tx) => tx.is_ready(),
        }
    }
}

impl<B: Payload + 'static> PoolClient<B> {
    //TODO: replace with `impl Future` when stable
    fn send_request_retryable(&mut self, req: Request<B>) -> Box<Future<Item=Response<Body>, Error=(::Error, Option<Request<B>>)> + Send>
    where
        B: Send,
    {
        match self.tx {
            PoolTx::Http1(ref mut tx) => tx.send_request_retryable(req),
            PoolTx::Http2(ref mut tx) => tx.send_request_retryable(req),
        }
    }
}

impl<B> Poolable for PoolClient<B>
where
    B: 'static,
{
    fn is_closed(&self) -> bool {
        match self.tx {
            PoolTx::Http1(ref tx) => tx.is_closed(),
            PoolTx::Http2(ref tx) => tx.is_closed(),
        }
    }

    fn reserve(self) -> Reservation<Self> {
        match self.tx {
            PoolTx::Http1(tx) => {
                Reservation::Unique(PoolClient {
                    is_proxied: self.is_proxied,
                    tx: PoolTx::Http1(tx),
                })
            },
            PoolTx::Http2(tx) => {
                let b = PoolClient {
                    is_proxied: self.is_proxied,
                    tx: PoolTx::Http2(tx.clone()),
                };
                let a = PoolClient {
                    is_proxied: self.is_proxied,
                    tx: PoolTx::Http2(tx),
                };
                Reservation::Shared(a, b)
            }
        }
    }
}

enum ClientError<B> {
    Normal(::Error),
    Canceled {
        connection_reused: bool,
        req: Request<B>,
        reason: ::Error,
    }
}

/// A marker to identify what version a pooled connection is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Ver {
    Http1,
    Http2,
}

fn set_relative_uri(uri: &mut Uri, is_proxied: bool) {
    if is_proxied && uri.scheme_part() != Some(&Scheme::HTTPS) {
        return;
    }
    let path = match uri.path_and_query() {
        Some(path) if path.as_str() != "/" => {
            let mut parts = ::http::uri::Parts::default();
            parts.path_and_query = Some(path.clone());
            Uri::from_parts(parts).expect("path is valid uri")
        },
        _none_or_just_slash => {
            "/".parse().expect("/ is valid path")
        }
    };
    *uri = path;
}

/// Builder for a Client
#[derive(Clone)]
pub struct Builder {
    //connect_timeout: Duration,
    exec: Exec,
    keep_alive: bool,
    keep_alive_timeout: Option<Duration>,
    h1_writev: bool,
    //TODO: make use of max_idle config
    max_idle: usize,
    retry_canceled_requests: bool,
    set_host: bool,
    ver: Ver,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            exec: Exec::Default,
            keep_alive: true,
            keep_alive_timeout: Some(Duration::from_secs(90)),
            h1_writev: true,
            max_idle: 5,
            retry_canceled_requests: true,
            set_host: true,
            ver: Ver::Http1,
        }
    }
}

impl Builder {
    /// Enable or disable keep-alive mechanics.
    ///
    /// Default is enabled.
    #[inline]
    pub fn keep_alive(&mut self, val: bool) -> &mut Self {
        self.keep_alive = val;
        self
    }

    /// Set an optional timeout for idle sockets being kept-alive.
    ///
    /// Pass `None` to disable timeout.
    ///
    /// Default is 90 seconds.
    #[inline]
    pub fn keep_alive_timeout(&mut self, val: Option<Duration>) -> &mut Self {
        self.keep_alive_timeout = val;
        self
    }

    /// Set whether HTTP/1 connections should try to use vectored writes,
    /// or always flatten into a single buffer.
    ///
    /// Note that setting this to false may mean more copies of body data,
    /// but may also improve performance when an IO transport doesn't
    /// support vectored writes well, such as most TLS implementations.
    ///
    /// Default is `true`.
    #[inline]
    pub fn http1_writev(&mut self, val: bool) -> &mut Self {
        self.h1_writev = val;
        self
    }

    /// Set whether the connection **must** use HTTP/2.
    ///
    /// Note that setting this to true prevents HTTP/1 from being allowed.
    ///
    /// Default is false.
    pub fn http2_only(&mut self, val: bool) -> &mut Self {
        self.ver = if val {
            Ver::Http2
        } else {
            Ver::Http1
        };
        self
    }

    /// Set whether to retry requests that get disrupted before ever starting
    /// to write.
    ///
    /// This means a request that is queued, and gets given an idle, reused
    /// connection, and then encounters an error immediately as the idle
    /// connection was found to be unusable.
    ///
    /// When this is set to `false`, the related `FutureResponse` would instead
    /// resolve to an `Error::Cancel`.
    ///
    /// Default is `true`.
    #[inline]
    pub fn retry_canceled_requests(&mut self, val: bool) -> &mut Self {
        self.retry_canceled_requests = val;
        self
    }

    /// Set whether to automatically add the `Host` header to requests.
    ///
    /// If true, and a request does not include a `Host` header, one will be
    /// added automatically, derived from the authority of the `Uri`.
    ///
    /// Default is `true`.
    #[inline]
    pub fn set_host(&mut self, val: bool) -> &mut Self {
        self.set_host = val;
        self
    }

    /// Provide an executor to execute background `Connection` tasks.
    pub fn executor<E>(&mut self, exec: E) -> &mut Self
    where
        E: Executor<Box<Future<Item=(), Error=()> + Send>> + Send + Sync + 'static,
    {
        self.exec = Exec::Executor(Arc::new(exec));
        self
    }

    /// Builder a client with this configuration and the default `HttpConnector`.
    #[cfg(feature = "runtime")]
    pub fn build_http<B>(&self) -> Client<HttpConnector, B>
    where
        B: Payload + Send,
        B::Data: Send,
    {
        let mut connector = HttpConnector::new(4);
        if self.keep_alive {
            connector.set_keepalive(self.keep_alive_timeout);
        }
        self.build(connector)
    }

    /// Combine the configuration of this builder with a connector to create a `Client`.
    pub fn build<C, B>(&self, connector: C) -> Client<C, B>
    where
        C: Connect,
        C::Transport: 'static,
        C::Future: 'static,
        B: Payload + Send,
        B::Data: Send,
    {
        Client {
            connector: Arc::new(connector),
            executor: self.exec.clone(),
            h1_writev: self.h1_writev,
            pool: Pool::new(self.keep_alive, self.keep_alive_timeout),
            retry_canceled_requests: self.retry_canceled_requests,
            set_host: self.set_host,
            ver: self.ver,
        }
    }
}

impl fmt::Debug for Builder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Builder")
            .field("keep_alive", &self.keep_alive)
            .field("keep_alive_timeout", &self.keep_alive_timeout)
            .field("http1_writev", &self.h1_writev)
            .field("max_idle", &self.max_idle)
            .field("set_host", &self.set_host)
            .field("version", &self.ver)
            .finish()
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn set_relative_uri_with_implicit_path() {
        let mut uri = "http://hyper.rs".parse().unwrap();
        set_relative_uri(&mut uri, false);

        assert_eq!(uri.to_string(), "/");
    }
}
