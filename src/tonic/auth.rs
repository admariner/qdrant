use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use storage::rbac::Access;
use tonic::Status;
use tonic::body::BoxBody;
use tower::{Layer, Service};

use crate::common::auth::{AuthError, AuthKeys};

type Request = tonic::codegen::http::Request<tonic::transport::Body>;
type Response = tonic::codegen::http::Response<BoxBody>;

#[derive(Clone)]
pub struct AuthMiddleware<S> {
    auth_keys: Arc<AuthKeys>,
    service: S,
}

async fn check(auth_keys: Arc<AuthKeys>, mut req: Request) -> Result<Request, Status> {
    // Allow health check endpoints to bypass authentication
    let path = req.uri().path();
    if path == "/qdrant.Qdrant/HealthCheck" || path == "/grpc.health.v1.Health/Check" {
        // Set default full access for health check endpoints
        let access = Access::full("Health check endpoints have full access without authentication");
        let inference_token = crate::common::inference::InferenceToken(None);

        req.extensions_mut().insert::<Access>(access);
        req.extensions_mut().insert(inference_token);

        return Ok(req);
    }

    let (access, inference_token) = auth_keys
        .validate_request(|key| req.headers().get(key).and_then(|val| val.to_str().ok()))
        .await
        .map_err(|e| match e {
            AuthError::Unauthorized(e) => Status::unauthenticated(e),
            AuthError::Forbidden(e) => Status::permission_denied(e),
            AuthError::StorageError(e) => Status::from(e),
        })?;

    let previous = req.extensions_mut().insert::<Access>(access);

    debug_assert!(
        previous.is_none(),
        "Previous access object should not exist in the request"
    );

    let previous_token = req.extensions_mut().insert(inference_token);

    debug_assert!(
        previous_token.is_none(),
        "Previous inference token should not exist in the request"
    );

    Ok(req)
}

impl<S> Service<Request> for AuthMiddleware<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let auth_keys = self.auth_keys.clone();
        let mut service = self.service.clone();

        Box::pin(async move {
            match check(auth_keys, request).await {
                Ok(req) => service.call(req).await,
                Err(e) => Ok(e.to_http()),
            }
        })
    }
}

#[derive(Clone)]
pub struct AuthLayer {
    auth_keys: Arc<AuthKeys>,
}

impl AuthLayer {
    pub fn new(auth_keys: AuthKeys) -> Self {
        Self {
            auth_keys: Arc::new(auth_keys),
        }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthMiddleware<S>;

    fn layer(&self, service: S) -> Self::Service {
        Self::Service {
            auth_keys: self.auth_keys.clone(),
            service,
        }
    }
}

pub fn extract_access<R>(req: &mut tonic::Request<R>) -> Access {
    req.extensions_mut().remove::<Access>().unwrap_or_else(|| {
        Access::full("All requests have full by default access when API key is not configured")
    })
}
