//! Bearer token authentication middleware for control endpoints.

use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
    Json,
};

/// Read the expected token once at startup.
pub fn get_auth_token() -> Option<String> {
    std::env::var("API_AUTH_TOKEN").ok().filter(|t| !t.is_empty())
}

/// Middleware: rejects requests without a valid Bearer token.
/// If API_AUTH_TOKEN is not set, all requests pass (paper-mode convenience).
pub async fn require_auth(
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let expected = get_auth_token();

    // No token configured → auth disabled (paper mode convenience)
    let Some(expected_token) = expected else {
        return Ok(next.run(request).await);
    };

    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let provided = auth_header.strip_prefix("Bearer ").unwrap_or("");

    if provided == expected_token {
        Ok(next.run(request).await)
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid or missing API_AUTH_TOKEN"})),
        ))
    }
}
