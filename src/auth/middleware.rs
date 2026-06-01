//! Auth middleware layer for axum.

use axum::extract::Request;
use axum::response::Response;
use std::collections::HashMap;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::{Layer, Service};

use super::protocol::Authenticator;

/// Key for storing identity in request extensions.
pub struct AuthIdentity;

pub static AUTH_IDENTITY: &str = "auth_identity";

#[derive(Clone)]
pub struct AuthMiddlewareLayer {
    authenticator: Arc<dyn Authenticator>,
    exempt_paths: Vec<String>,
}

impl AuthMiddlewareLayer {
    pub fn new(authenticator: Arc<dyn Authenticator>, exempt_paths: Vec<String>) -> Self {
        Self {
            authenticator,
            exempt_paths,
        }
    }
}

impl<S> Layer<S> for AuthMiddlewareLayer {
    type Service = AuthMiddlewareService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthMiddlewareService {
            inner,
            authenticator: self.authenticator.clone(),
            exempt_paths: self.exempt_paths.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthMiddlewareService<S> {
    inner: S,
    authenticator: Arc<dyn Authenticator>,
    exempt_paths: Vec<String>,
}

impl<S> Service<Request> for AuthMiddlewareService<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let path = req.uri().path().to_string();
        let exempt = self
            .exempt_paths
            .iter()
            .any(|p| path.starts_with(p) || path == *p);

        let authenticator = self.authenticator.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let mut req = req;
            if exempt {
                return inner.call(req).await;
            }

            let headers: HashMap<String, String> = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            match authenticator.authenticate(&headers).await {
                None => {
                    let resp = Response::builder()
                        .status(401)
                        .body(axum::body::Body::from("Unauthorized"))
                        .unwrap();
                    Ok(resp)
                }
                Some(identity) => {
                    // Expose the authenticated identity to handlers (the
                    // `AuthIdentity` extractor reads it from extensions) so it
                    // flows into the apcore Context.
                    req.extensions_mut().insert(identity);
                    inner.call(req).await
                }
            }
        })
    }
}
