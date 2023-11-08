//! Subscription/Notification Function Set

use anyhow::{Context, Result};
use dashmap::DashMap;
use hyper::{server::conn::Http, service::service_fn, Body, Method, Request, Response};
use openssl::ssl::Ssl;
use sep2_common::{deserialize, packages::pubsub::Notification, traits::SEResource};
use std::net;
use std::path::Path;
use std::{future::Future, net::SocketAddr, pin::Pin, sync::Arc};
use tokio::net::TcpListener;
use tokio_openssl::SslStream;

use crate::client::SEPResponse;
use crate::tls::{create_server_tls_config, TlsServerConfig};

/// A trait implemented by types that can be used as a route callback in a [`ClientNotifServer`].
///
///
// This trait uses extra heap allocations while we await stable RPITIT (and eventually async fn with a send bound future)
pub trait RouteCallback<T: SEResource>: Send + Sync + 'static {
    fn callback(
        &self,
        notif: Notification<T>,
    ) -> Pin<Box<dyn Future<Output = SEPResponse> + Send + 'static>>;
}

/// Automatically implemented for all [`Fn`] with a matching function signature.
impl<F, R, T: SEResource> RouteCallback<T> for F
where
    F: Fn(Notification<T>) -> R + Send + Sync + 'static,
    R: Future<Output = SEPResponse> + Send + 'static,
{
    fn callback(
        &self,
        notif: Notification<T>,
    ) -> Pin<Box<dyn Future<Output = SEPResponse> + Send + 'static>> {
        Box::pin(self(notif))
    }
}

/// Internal Boxed future version of a RouteCallback
type RouteHandler = Box<
    dyn Fn(&str) -> Pin<Box<dyn Future<Output = SEPResponse> + Send + 'static>>
        + Send
        + Sync
        + 'static,
>;

struct Router {
    // We use dashmap to allow for multiple concurrent lookups into the map.
    // The map is never resized once the server is started, so it doesn't make sense to use a big lock.
    // It's likely a NotifServer will never store segmented paths, so a hash lookup is acceptable.
    routes: DashMap<String, RouteHandler>,
}

impl Router {
    fn new() -> Self {
        Router {
            routes: DashMap::new(),
        }
    }

    async fn router(&self, req: Request<Body>) -> Result<Response<Body>> {
        let path = req.uri().path().to_owned();
        match self.routes.get_mut(&path) {
            Some(mut func) => {
                let method = req.method();
                match method {
                    &Method::POST => {
                        let body = req.into_body();
                        let bytes = hyper::body::to_bytes(body).await?;
                        let xml = String::from_utf8(bytes.to_vec())?;
                        let func = func.value_mut();
                        Ok(hyper::Response::try_from(func(&xml).await)?)
                    }
                    _ => {
                        hyper::Response::try_from(SEPResponse::MethodNotAllowed("POST".to_owned()))
                    }
                }
            }
            None => hyper::Response::try_from(SEPResponse::NotFound),
        }
    }
}

/// A lightweight IEEE 2030.5 Server for receiving [`Notification<T>`] resources from a server for the subscription / notification mechanism.
pub struct ClientNotifServer {
    addr: SocketAddr,
    cfg: TlsServerConfig,
    router: Router,
}

impl ClientNotifServer {
    /// Create a new Notification server that listens on the given address
    pub fn new(
        addr: impl net::ToSocketAddrs,
        cert_path: impl AsRef<Path>,
        pk_path: impl AsRef<Path>,
        rootca_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let cfg = create_server_tls_config(cert_path, pk_path, rootca_path)?;
        Ok(ClientNotifServer {
            addr: addr
                .to_socket_addrs()?
                .next()
                .context("Given server address did not yield a SocketAddr")?,
            cfg,
            router: Router::new(),
        })
    }

    /// Add a route to the server.
    /// Given:
    /// - A relative URI of the form "/foo"
    /// - A `Fn` callback accepting a [`Notification<T>`]`, where T is the expected [`SEResource`] on the route  
    ///
    /// The `RouteCallback` trait can be implemented on any threadsafe type,
    /// however it is automatically implemented for any applicable 'Fn'
    pub fn add<T>(self, path: impl Into<String>, callback: impl RouteCallback<T>) -> Self
    where
        T: SEResource,
    {
        let path = path.into();
        let log_path = path.clone();
        let new: RouteHandler = Box::new(move |e| {
            let e = deserialize::<Notification<T>>(e);
            match e {
                Ok(resource) => {
                    log::debug!("NotifServer: Successfully deserialized a resource on {log_path}");
                    Box::pin(callback.callback(resource))
                }
                Err(err) => {
                    log::error!("NotifServer: Failed to deserialize resource on {log_path}: {err}");
                    Box::pin(async { SEPResponse::BadRequest(None) })
                }
            }
        });
        self.router.routes.insert(path, new);
        self
    }

    /// Start the Notification Server.
    ///
    /// When the provided `shutdown` future completes, the server will shutdown gracefully.
    ///
    /// This function will return an error IFF the server could not be started.
    /// It will recover from all other errors.
    pub async fn run(self, shutdown: impl Future) -> Result<()> {
        tokio::pin!(shutdown);
        let acceptor = self.cfg.build();
        let router = Arc::new(self.router);
        let listener = TcpListener::bind(self.addr).await?;
        let mut set = tokio::task::JoinSet::new();
        log::info!("NotifServer: Listening on {}", self.addr);
        loop {
            // Accept TCP Connection
            let (stream, addr) = tokio::select! {
                _ = &mut shutdown => break,
                res = listener.accept() => match res {
                    Ok((s,a)) => (s,a),
                    Err(err) => {
                        log::error!("NotifServer: Failed to accept connection: {err}");
                        continue;
                    }
                }
            };
            log::debug!("NotifServer: Remote connecting from {}", addr);

            // Perform TLS handshake
            let ssl = Ssl::new(acceptor.context())?;
            let stream = SslStream::new(ssl, stream)?;
            let mut stream = Box::pin(stream);
            if let Err(e) = stream.as_mut().accept().await {
                log::error!("NotifServer: Failed to perform TLS handshake: {e}");
                continue;
            }

            // Bind connection to service
            let router = router.clone();
            let service = service_fn(move |req| {
                let handler = router.clone();
                async move { handler.router(req).await }
            });
            set.spawn(async move {
                if let Err(err) = Http::new().serve_connection(stream, service).await {
                    log::error!("NotifServer: Failed to handle connection: {err}");
                }
            });
        }
        // Wait for all connection handlers to finish
        log::debug!("NotifServer: Attempting graceful shutdown");
        set.shutdown().await;
        log::info!("NotifServer: Server has been shutdown.");
        Ok(())
    }
}
