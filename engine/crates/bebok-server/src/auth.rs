//! Capability token: `Authorization: Bearer <token>` or `?token=<token>`
//! (EventSource cannot send headers, so SSE uses the query form).

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::routes::error;
use crate::state::SharedState;

pub async fn require_token(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.config.token.as_deref() else {
        return next.run(req).await;
    };
    if req.method() == Method::OPTIONS {
        return next.run(req).await;
    }
    let bearer =
        req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let query = req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")));
    if bearer.or(query).is_some_and(|t| constant_eq(t.as_bytes(), expected.as_bytes())) {
        next.run(req).await
    } else {
        error(StatusCode::UNAUTHORIZED, "missing or invalid token").into_response()
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
