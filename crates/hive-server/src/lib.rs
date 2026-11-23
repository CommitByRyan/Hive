pub mod error;
mod routes;
pub mod sse;
mod state;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

pub use error::ApiError;
pub use state::AppState;

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_URI_BYTES: usize = 2 * 1024;
pub const SSE_QUEUE_CAPACITY: usize = 256;

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(routes::health))
        .route("/state", get(routes::state))
        .route(
            "/missions",
            get(routes::list_missions).post(routes::create_mission),
        )
        .route("/missions/{id}", get(routes::get_mission))
        .route("/missions/{id}/messages", post(routes::post_message))
        .route("/events", get(routes::events))
        .route("/approvals/{id}", post(routes::decide_approval))
        .route("/artifacts/{id}", get(routes::get_artifact))
        .route(
            "/routines",
            get(routes::list_routines).post(routes::create_routine),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api_guard,
        ));

    Router::new()
        .nest("/api", api)
        .route("/", get(index))
        .route("/index.html", get(index))
        .with_state(state)
}

async fn index() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(include_str!("../web/index.html").to_owned()))
        .expect("valid html response")
}

async fn api_guard(State(state): State<AppState>, request: Request<Body>, next: Next) -> Response {
    let uri = request.uri().to_string();
    if uri.len() > MAX_URI_BYTES {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request URI exceeds 2 KiB",
        );
    }

    if let Some(origin) = request.headers().get(header::ORIGIN) {
        if !origin_allowed(origin, &state) {
            return json_error(
                StatusCode::FORBIDDEN,
                "origin_forbidden",
                "origin is not allowed",
            );
        }
    }

    if is_write(request.method()) {
        if let Some(content_length) = request.headers().get(header::CONTENT_LENGTH) {
            if let Ok(length) = content_length.to_str().unwrap_or("0").parse::<usize>() {
                if length > MAX_BODY_BYTES {
                    return json_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "payload_too_large",
                        "request body exceeds 64 KiB",
                    );
                }
            }
        }
        if !is_json_content_type(request.headers().get(header::CONTENT_TYPE)) {
            return json_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "write requests must use application/json",
            );
        }
    }

    let mut response = next.run(request).await.into_response();
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn is_write(method: &Method) -> bool {
    matches!(
        method,
        &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
    )
}

fn is_json_content_type(value: Option<&HeaderValue>) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let value = value.split(';').next().unwrap_or("").trim();
            value == "application/json"
        })
}

fn origin_allowed(origin: &HeaderValue, state: &AppState) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(uri) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    let Some(scheme) = uri.scheme_str() else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let Some(host) = uri.host() else {
        return false;
    };
    if !matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]") {
        return false;
    }
    if let Some(port) = uri.port_u16() {
        port == state.port
    } else {
        true
    }
}

fn json_error(status: StatusCode, code: &'static str, message: &str) -> Response {
    json_response(
        status,
        json!({
            "error": {
                "code": code,
                "message": message,
                "details": {},
            }
        }),
    )
}

pub fn json_response(status: StatusCode, body: Value) -> Response {
    let bytes = serde_json::to_vec(&body).expect("json value serializes");
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(bytes))
        .expect("valid response")
}
