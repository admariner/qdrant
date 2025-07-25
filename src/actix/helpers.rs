use std::fmt::Debug;
use std::future::Future;

use actix_web::http::header;
use actix_web::http::header::HeaderMap;
use actix_web::rt::time::Instant;
use actix_web::{HttpResponse, ResponseError, http};
use api::rest::models::{ApiResponse, ApiStatus, HardwareUsage, InferenceUsage, Usage};
use collection::operations::types::CollectionError;
use common::counter::hardware_accumulator::HwMeasurementAcc;
use serde::Serialize;
use storage::content_manager::errors::StorageError;
use storage::content_manager::toc::request_hw_counter::RequestHwCounter;
use storage::dispatcher::Dispatcher;

pub fn get_request_hardware_counter(
    dispatcher: &Dispatcher,
    collection_name: String,
    report_to_api: bool,
    wait: Option<bool>,
) -> RequestHwCounter {
    let report_to_api = report_to_api && wait != Some(false);
    RequestHwCounter::new(
        HwMeasurementAcc::new_with_metrics_drain(
            dispatcher.get_collection_hw_metrics(collection_name),
        ),
        report_to_api,
    )
}

pub fn accepted_response(
    timing: Instant,
    hardware_usage: Option<HardwareUsage>,
    inference_usage: Option<InferenceUsage>,
) -> HttpResponse {
    let usage = {
        let u = Usage {
            hardware: hardware_usage,
            inference: inference_usage,
        };
        if u.is_empty() { None } else { Some(u) }
    };

    HttpResponse::Accepted().json(ApiResponse::<()> {
        result: None,
        status: ApiStatus::Accepted,
        time: timing.elapsed().as_secs_f64(),
        usage,
    })
}

pub fn process_response_with_inference_usage<T>(
    response: Result<T, StorageError>,
    timing: Instant,
    hardware_usage: Option<HardwareUsage>,
    inference_usage: Option<InferenceUsage>,
) -> HttpResponse
where
    T: Serialize,
{
    match response {
        Ok(res) => HttpResponse::Ok().json(ApiResponse {
            result: Some(res),
            status: ApiStatus::Ok,
            time: timing.elapsed().as_secs_f64(),
            usage: Some(Usage {
                hardware: hardware_usage,
                inference: inference_usage,
            }),
        }),
        Err(err) => process_response_error_with_inference_usage(
            err,
            timing,
            hardware_usage,
            inference_usage,
        ),
    }
}

pub fn process_response<T>(
    response: Result<T, StorageError>,
    timing: Instant,
    hardware_usage: Option<HardwareUsage>,
) -> HttpResponse
where
    T: Serialize,
{
    process_response_with_inference_usage(response, timing, hardware_usage, None)
}

pub fn process_response_error_with_inference_usage(
    err: StorageError,
    timing: Instant,
    hardware_usage: Option<HardwareUsage>,
    inference_usage: Option<InferenceUsage>,
) -> HttpResponse {
    log_service_error(&err);

    let error = HttpError::from(err);
    let http_code = error.status_code();
    let headers = error.headers();
    let json_body = ApiResponse::<()> {
        result: None,
        status: ApiStatus::Error(error.to_string()),
        time: timing.elapsed().as_secs_f64(),
        usage: Some(Usage {
            hardware: hardware_usage,
            inference: inference_usage,
        }),
    };

    let mut response_builder = HttpResponse::build(http_code);
    for header_pair in headers {
        response_builder.insert_header(header_pair);
    }
    response_builder.json(json_body)
}

pub fn process_response_error(
    err: StorageError,
    timing: Instant,
    hardware_usage: Option<HardwareUsage>,
) -> HttpResponse {
    process_response_error_with_inference_usage(err, timing, hardware_usage, None)
}

pub fn already_in_progress_response() -> HttpResponse {
    HttpResponse::build(http::StatusCode::SERVICE_UNAVAILABLE).json(ApiResponse::<()> {
        result: None,
        status: ApiStatus::AlreadyInProgress,
        time: 0.0,
        usage: None,
    })
}

/// Response wrapper for a `Future` returning `Result`.
///
/// # Cancel safety
///
/// Future must be cancel safe.
pub async fn time<T, Fut>(future: Fut) -> HttpResponse
where
    Fut: Future<Output = Result<T, StorageError>>,
    T: serde::Serialize,
{
    time_impl(async { future.await.map(Some) }).await
}

/// Response wrapper for a `Future` returning `Result`.
/// If `wait` is false, returns `202 Accepted` immediately.
pub async fn time_or_accept<T, Fut>(future: Fut, wait: bool) -> HttpResponse
where
    Fut: Future<Output = Result<T, StorageError>> + Send + 'static,
    T: serde::Serialize + Send + 'static,
{
    let future = async move {
        let handle = tokio::task::spawn(async move {
            let result = future.await;

            if !wait {
                if let Err(err) = &result {
                    log_service_error(err);
                }
            }

            result
        });

        if wait {
            handle.await?.map(Some)
        } else {
            Ok(None)
        }
    };

    time_impl(future).await
}

/// # Cancel safety
///
/// Future must be cancel safe.
async fn time_impl<T, Fut>(future: Fut) -> HttpResponse
where
    Fut: Future<Output = Result<Option<T>, StorageError>>,
    T: serde::Serialize,
{
    let instant = Instant::now();
    match future.await.transpose() {
        Some(res) => process_response(res, instant, None),
        None => accepted_response(instant, None, None),
    }
}

fn log_service_error(err: &StorageError) {
    if let StorageError::ServiceError { backtrace, .. } = err {
        log::error!("Error processing request: {err}");

        if let Some(backtrace) = backtrace {
            log::trace!("Backtrace: {backtrace}");
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(StorageError);

impl HttpError {
    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        match &self.0 {
            StorageError::RateLimitExceeded {
                description: _,
                retry_after,
            } => {
                if let Some(retry_after) = retry_after {
                    // Retry-After is expressed in seconds `https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Retry-After`
                    // Ceil the value to the nearest second so clients don't retry too early
                    let retry_after_sec = retry_after.as_secs_f32().ceil() as u32;
                    headers.insert(
                        header::RETRY_AFTER,
                        header::HeaderValue::from(retry_after_sec),
                    );
                }
            }
            StorageError::BadInput { .. } => {}
            StorageError::AlreadyExists { .. } => {}
            StorageError::NotFound { .. } => {}
            StorageError::ServiceError { .. } => {}
            StorageError::BadRequest { .. } => {}
            StorageError::Locked { .. } => {}
            StorageError::Timeout { .. } => {}
            StorageError::ChecksumMismatch { .. } => {}
            StorageError::Forbidden { .. } => {}
            StorageError::PreconditionFailed { .. } => {}
            StorageError::InferenceError { .. } => {}
            StorageError::ShardUnavailable { .. } => {}
        }
        headers
    }
}

impl ResponseError for HttpError {
    fn status_code(&self) -> http::StatusCode {
        match &self.0 {
            StorageError::BadInput { .. } => http::StatusCode::BAD_REQUEST,
            StorageError::NotFound { .. } => http::StatusCode::NOT_FOUND,
            StorageError::ServiceError { .. } => http::StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::BadRequest { .. } => http::StatusCode::BAD_REQUEST,
            StorageError::Locked { .. } => http::StatusCode::FORBIDDEN,
            StorageError::Timeout { .. } => http::StatusCode::REQUEST_TIMEOUT,
            StorageError::AlreadyExists { .. } => http::StatusCode::CONFLICT,
            StorageError::ChecksumMismatch { .. } => http::StatusCode::BAD_REQUEST,
            StorageError::Forbidden { .. } => http::StatusCode::FORBIDDEN,
            StorageError::PreconditionFailed { .. } => http::StatusCode::INTERNAL_SERVER_ERROR,
            StorageError::InferenceError { .. } => http::StatusCode::BAD_REQUEST,
            StorageError::RateLimitExceeded { .. } => http::StatusCode::TOO_MANY_REQUESTS,
            StorageError::ShardUnavailable { .. } => http::StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl From<StorageError> for HttpError {
    fn from(err: StorageError) -> Self {
        HttpError(err)
    }
}

impl From<CollectionError> for HttpError {
    fn from(err: CollectionError) -> Self {
        HttpError(err.into())
    }
}

impl From<std::io::Error> for HttpError {
    fn from(err: std::io::Error) -> Self {
        HttpError(err.into()) // TODO: Is this good enough?.. 🤔
    }
}
