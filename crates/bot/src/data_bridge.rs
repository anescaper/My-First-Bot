//! HTTP bridge to the Python strategy service.
//!
//! Originally this module contained DataHub setup code, which has since moved
//! to the standalone `data-hub` service binary. Now it only contains the
//! `call_strategy_service` function for forwarding prediction requests to Python.
//!
//! The bot communicates with the Python service via HTTP POST to `/predict`.
//! The service runs on `STRATEGY_SERVICE_URL` (default http://localhost:8100).

/// Call the Python strategy service for a prediction.
///
/// Sends a JSON payload to `{base_url}/predict` and parses the response.
/// Returns None if:
/// - The service is unreachable (network error, service down)
/// - The request times out (hardcoded 5-second timeout — long enough for Python
///   GARCH/regression computations, short enough to not block the pipeline cycle)
/// - The response is not valid JSON
/// - The HTTP status is non-2xx
///
/// The caller (pipeline.rs `try_python_prediction`) is responsible for
/// building the payload and parsing the response fields.
pub async fn call_strategy_service(
    client: &reqwest::Client,
    base_url: &str,
    payload: &serde_json::Value,
) -> Option<serde_json::Value> {
    let url = format!("{}/predict", base_url);
    match client
        .post(&url)
        .json(payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp.json().await.ok(),
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "Strategy service error");
            None
        }
        Err(e) => {
            tracing::warn!("Strategy service unavailable: {e}");
            None
        }
    }
}
