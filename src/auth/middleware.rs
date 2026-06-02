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
    require_auth: bool,
}

impl AuthMiddlewareLayer {
    /// Create a layer in strict mode (`require_auth = true`): unauthenticated
    /// non-exempt requests are rejected with 401.
    pub fn new(authenticator: Arc<dyn Authenticator>, exempt_paths: Vec<String>) -> Self {
        Self {
            authenticator,
            exempt_paths,
            require_auth: true,
        }
    }

    /// Create a layer with an explicit `require_auth` flag.
    ///
    /// When `require_auth` is `false` (permissive mode), unauthenticated
    /// non-exempt requests proceed downstream with no `Identity` inserted
    /// (identity = None) instead of returning 401.
    pub fn with_require_auth(
        authenticator: Arc<dyn Authenticator>,
        exempt_paths: Vec<String>,
        require_auth: bool,
    ) -> Self {
        Self {
            authenticator,
            exempt_paths,
            require_auth,
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
            require_auth: self.require_auth,
        }
    }
}

#[derive(Clone)]
pub struct AuthMiddlewareService<S> {
    inner: S,
    authenticator: Arc<dyn Authenticator>,
    exempt_paths: Vec<String>,
    require_auth: bool,
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
        let require_auth = self.require_auth;
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
                    if require_auth {
                        let resp = Response::builder()
                            .status(401)
                            .body(axum::body::Body::from("Unauthorized"))
                            .unwrap();
                        Ok(resp)
                    } else {
                        // Permissive mode: proceed with no identity inserted
                        // (identity = None) instead of returning 401.
                        inner.call(req).await
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use apcore::context::Identity;
    use async_trait::async_trait;
    use axum::body::Body;
    use serde_json::Value;
    use tower::ServiceExt;

    /// Authenticator that always fails (returns None).
    struct AlwaysNone;

    #[async_trait]
    impl Authenticator for AlwaysNone {
        async fn authenticate(&self, _headers: &HashMap<String, String>) -> Option<Identity> {
            None
        }
        fn security_schemes(&self) -> Option<Value> {
            None
        }
    }

    fn ok_service() -> impl Service<
        Request,
        Response = Response,
        Error = std::convert::Infallible,
        Future = impl std::future::Future<Output = Result<Response, std::convert::Infallible>> + Send,
    > + Clone {
        tower::service_fn(|_req: Request| async move {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(200)
                    .body(Body::from("downstream"))
                    .unwrap(),
            )
        })
    }

    #[tokio::test]
    async fn require_auth_true_rejects_with_401() {
        let layer = AuthMiddlewareLayer::new(Arc::new(AlwaysNone), vec![]);
        let svc = layer.layer(ok_service());
        let req = Request::builder()
            .uri("/jsonrpc")
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn require_auth_false_permits_downstream() {
        let layer = AuthMiddlewareLayer::with_require_auth(Arc::new(AlwaysNone), vec![], false);
        let svc = layer.layer(ok_service());
        let req = Request::builder()
            .uri("/jsonrpc")
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        // Permissive mode: unauthenticated non-exempt request runs downstream.
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn exempt_path_skips_auth() {
        let layer =
            AuthMiddlewareLayer::new(Arc::new(AlwaysNone), vec!["/.well-known/".to_string()]);
        let svc = layer.layer(ok_service());
        let req = Request::builder()
            .uri("/.well-known/agent-card.json")
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }
}
