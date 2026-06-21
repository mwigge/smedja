use crate::{SreConfig, SreError};

/// Returns the current Unix timestamp in seconds.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Queries the Prometheus range query API with a `PromQL` expression.
///
/// Sends a `GET` request to
/// `{prometheus_endpoint}/api/v1/query_range?query={promql}&start=...&end=...&step=60`.
/// The `start` and `end` parameters are derived from the current time and
/// `time_range_minutes`.
///
/// # Errors
///
/// - [`SreError::Http`] if the HTTP transport fails.
/// - [`SreError::ApiError`] if the server returns a non-2xx status code.
pub async fn metric_query(
    client: &reqwest::Client,
    config: &SreConfig,
    promql: &str,
    time_range_minutes: u32,
) -> Result<serde_json::Value, SreError> {
    let end = now_unix();
    let start = end.saturating_sub(u64::from(time_range_minutes) * 60);

    let url = format!(
        "{}/api/v1/query_range?query={promql}&start={start}&end={end}&step=60",
        config.prometheus_endpoint,
    );

    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(SreError::ApiError { status, body });
    }

    let json = response.json::<serde_json::Value>().await?;
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[tokio::test]
    async fn metric_query_sends_correct_request() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"/api/v1/query_range\?query=".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"status": "success", "data": {}}"#)
            .create_async()
            .await;

        let config = SreConfig::new("http://unused:3301", server.url(), "http://unused:3100");
        let client = reqwest::Client::new();

        let result = metric_query(&client, &config, "up", 10)
            .await
            .expect("metric_query should succeed");

        mock.assert_async().await;
        assert_eq!(result["status"], "success");
    }

    #[tokio::test]
    async fn metric_query_propagates_api_error() {
        let mut server = Server::new_async().await;
        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"/api/v1/query_range".to_string()),
            )
            .with_status(500)
            .with_body("prometheus unavailable")
            .create_async()
            .await;

        let config = SreConfig::new("http://unused:3301", server.url(), "http://unused:3100");
        let client = reqwest::Client::new();

        let err = metric_query(&client, &config, "up", 5)
            .await
            .expect_err("metric_query should propagate 500");

        assert!(
            matches!(err, SreError::ApiError { status: 500, .. }),
            "expected ApiError(500), got: {err}"
        );
    }
}
