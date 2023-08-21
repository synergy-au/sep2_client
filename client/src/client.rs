use anyhow::{anyhow, bail, Context, Result};
use common::{
    deserialize,
    packages::{primitives::Uint32, traits::SEResource},
    serialize,
};
use hyper::{
    header::{ACCEPT, ALLOW, CONTENT_LENGTH, CONTENT_TYPE, LOCATION},
    http::HeaderValue,
    Body, Method, Request, StatusCode, Uri,
};
use log::{debug, error, info};
use std::{future::Future, time::Duration};
use tokio::sync::broadcast::{self, Sender};

use crate::tls::{create_client, create_client_tls_cfg, HTTPSClient};

/// Possible HTTP Responses for a IEE 2030.5 Client to both send & receive.
pub enum SepResponse {
    // HTTP 201 w/ Location header value - 2030.5-2018 - 5.5.2.4
    Created(String),
    // HTTP 204 - 2030.5-2018 - 5.5.2.5
    NoContent,
    // HTTP 400 - 2030.5-2018 - 5.5.2.9
    BadRequest,
    // HTTP 404 - 2030.5-2018 - 5.5.2.11
    NotFound,
    // HTTP 405 w/ Allow header value - 2030.5-2018 - 5.5.2.12
    MethodNotAllowed(&'static str),
}

impl From<SepResponse> for hyper::Response<Body> {
    fn from(value: SepResponse) -> Self {
        let mut res = hyper::Response::new(Body::empty());
        match value {
            SepResponse::Created(loc) => {
                *res.status_mut() = StatusCode::CREATED;
                res.headers_mut().insert(LOCATION, loc.parse().unwrap());
            }
            SepResponse::NoContent => {
                *res.status_mut() = StatusCode::NO_CONTENT;
            }
            SepResponse::BadRequest => {
                *res.status_mut() = StatusCode::BAD_REQUEST;
            }
            SepResponse::NotFound => {
                *res.status_mut() = StatusCode::NOT_FOUND;
            }
            SepResponse::MethodNotAllowed(methods) => {
                *res.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                res.headers_mut()
                    .insert(ALLOW, HeaderValue::from_static(methods));
            }
        };
        res
    }
}

#[derive(Clone)]
enum PollTask {
    ForceRun,
    Cancel,
}
/// Represents an IEEE 2030.5 Client.
#[derive(Clone)]
pub struct Client {
    addr: String,
    http: HTTPSClient,
    broadcaster: Sender<PollTask>,
}

impl Client {
    const DEFAULT_POLLRATE: Uint32 = Uint32(900);

    pub fn new(
        server_addr: &str,
        cert_path: &str,
        pk_path: &str,
        tcp_keepalive: Option<Duration>,
    ) -> Result<Self> {
        let cfg = create_client_tls_cfg(cert_path, pk_path)?;
        let (tx, _) = broadcast::channel::<PollTask>(1);
        Ok(Client {
            addr: server_addr.to_owned(),
            http: create_client(cfg, tcp_keepalive),
            broadcaster: tx,
        })
    }

    /// Retrieve the resource at the given path.
    ///
    /// Returns an error if the resource could not be retrieved or deserialized.
    pub async fn get<R: SEResource>(&self, path: &str) -> Result<R> {
        let uri: Uri = format!("{}{}", self.addr, path)
            .parse()
            .context("Failed to parse address")?;
        info!("GET {} from {}", R::name(), uri);
        let req = Request::builder()
            .method(Method::GET)
            .header(ACCEPT, "application/sep+xml")
            .uri(uri)
            .body(Body::default())?;
        debug!("Outgoing HTTP Request: {:?}", req);
        let res = self.http.request(req).await?;
        debug!("Incoming HTTP Response: {:?}", res);
        // TODO: Improve error handling
        match res.status() {
            StatusCode::OK => (),
            StatusCode::NOT_FOUND => bail!("404 Not Found"),
            _ => bail!("Unexpected HTTP response from server"),
        }
        let body = hyper::body::to_bytes(res.into_body()).await?;
        let xml = String::from_utf8_lossy(&body);
        deserialize(&xml)
    }

    /// Update a resource at the given path.
    ///
    /// Returns an error if the server does not respond with 204 No Content or 201 Created.
    pub async fn post<R: SEResource>(&self, path: &str, resource: &R) -> Result<SepResponse> {
        self.put_post(path, resource, Method::POST).await
    }

    /// Create a resource at the given path.
    ///
    /// Returns an error if the server does not respond with 204 No Content or 201 Created.
    pub async fn put<R: SEResource>(&self, path: &str, resource: &R) -> Result<SepResponse> {
        self.put_post(path, resource, Method::PUT).await
    }

    /// Delete the resource at the given path.
    ///
    /// Returns an error if the server does not respond with 204 No Content.
    pub async fn delete(&self, path: &str) -> Result<()> {
        let uri: Uri = format!("{}{}", self.addr, path)
            .parse()
            .context("Failed to parse address")?;
        info!("DELETE at {}", uri);
        let req = Request::builder()
            .method(Method::DELETE)
            .uri(uri)
            .body(Body::empty())?;
        debug!("Outgoing HTTP Request: {:?}", req);
        let res = self.http.request(req).await?;
        debug!("Incoming HTTP Response: {:?}", res);
        // TODO: Improve error handling
        match res.status() {
            StatusCode::NO_CONTENT => Ok(()),
            StatusCode::BAD_REQUEST => bail!("400 Bad Request"),
            StatusCode::NOT_FOUND => bail!("404 Not Found"),
            _ => bail!("Unexpected HTTP response from server"),
        }
    }

    /// Begin polling the given route on a regular interval, passing the returned resource to the given callback.
    ///
    /// As per IEEE 2030.5, if a poll rate is not specified, a default of 900 seconds (15 minutes) is used.
    ///
    /// All poll events created can be forcibly run using `force_poll`.
    pub async fn start_poll<R: SEResource, F, Res>(
        &self,
        path: String,
        poll_rate: Option<Uint32>,
        mut callback: F,
    ) where
        R: SEResource + Send,
        F: FnMut(R) -> Res + Send + Sync + 'static,
        Res: Future<Output = ()> + Send,
    {
        let client = self.clone();
        let mut rx = self.broadcaster.subscribe();
        tokio::task::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(
                        *poll_rate.unwrap_or(Self::DEFAULT_POLLRATE) as u64,
                    )) => (),
                    flag = rx.recv() => {
                        if let Ok(v) = flag {
                            match v {
                                PollTask::ForceRun => (),
                                PollTask::Cancel => break,
                            }
                        } else {
                            break;
                        }
                    }
                }
                let res = client.get::<R>(&path).await;
                match res {
                    Ok(rsrc) => {
                        info!("Scheduled poll for Resource {} successful.", R::name());
                        callback(rsrc).await;
                    }
                    Err(_) => error!(
                        "Scheduled poll for Resource {} at {} failed. Retrying in {} seconds.",
                        R::name(),
                        &path,
                        &poll_rate.unwrap_or(Self::DEFAULT_POLLRATE)
                    ),
                }
            }
        });
    }

    /// Forcibly poll & run the callbacks of all routes polled using `start_poll`
    pub async fn force_poll(&self) {
        let _ = self.broadcaster.send(PollTask::ForceRun);
    }

    /// Cancel all poll tasks created using `start_poll`
    pub async fn cancel_polls(&self) {
        let _ = self.broadcaster.send(PollTask::Cancel);
    }

    // Create a PUT or POST request
    async fn put_post<R: SEResource>(
        &self,
        path: &str,
        resource: &R,
        method: Method,
    ) -> Result<SepResponse> {
        let uri: Uri = format!("{}{}", self.addr, path)
            .parse()
            .context("Failed to parse address")?;
        info!("POST {} to {}", R::name(), uri);
        let rsrce = serialize(resource)?;
        let rsrce_size = rsrce.as_bytes().len();
        let req = Request::builder()
            .method(method)
            .header(CONTENT_TYPE, "application/sep+xml")
            .header(CONTENT_LENGTH, rsrce_size)
            .uri(uri)
            .body(Body::from(rsrce))?;
        debug!("Outgoing HTTP Request: {:?}", req);
        let res = self.http.request(req).await?;
        debug!("Incoming HTTP Response: {:?}", res);
        // TODO: Improve error handling
        match res.status() {
            StatusCode::CREATED => {
                let loc = res
                    .headers()
                    .get(LOCATION)
                    .ok_or(anyhow!("201 Created - Missing Location Header"))?
                    .to_str()?
                    .to_string();
                Ok(SepResponse::Created(loc))
            }
            StatusCode::NO_CONTENT => Ok(SepResponse::NoContent),
            StatusCode::BAD_REQUEST => bail!("400 Bad Request"),
            StatusCode::NOT_FOUND => bail!("404 Not Found"),
            _ => bail!("Unexpected HTTP response from server"),
        }
    }
}