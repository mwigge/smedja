use crate::{SreConfig, SreError};

/// Tails logs from Loki matching a `LogQL` filter.
///
/// Sends a `GET` request to
/// `{loki_endpoint}/loki/api/v1/query_range?query={filter}&limit={lines}`.
/// The `service` label is prepended to the `filter` expression as
/// `{service="{service}"}` when the filter does not already begin with `{`.
///
/// # Errors
///
/// - [`SreError::Http`] if the HTTP transport fails.
/// - [`SreError::ApiError`] if the server returns a non-2xx status code.
pub async fn log_tail(
    client: &reqwest::Client,
    config: &SreConfig,
    service: &str,
    filter: &str,
    lines: u32,
) -> Result<serde_json::Value, SreError> {
    // Build a LogQL stream selector that always includes the service label.
    let query = if filter.starts_with('{') {
        filter.to_owned()
    } else {
        format!(r#"{{service="{service}"}}{filter}"#)
    };

    let url = format!(
        "{}/loki/api/v1/query_range?query={query}&limit={lines}",
        config.loki_endpoint,
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
    async fn log_tail_sends_correct_request() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"/loki/api/v1/query_range\?.*limit=".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"status": "200", "data": {}}"#)
            .create_async()
            .await;

        let config = SreConfig::new("http://unused:3301", "http://unused:9090", server.url());
        let client = reqwest::Client::new();

        let result = log_tail(&client, &config, "api-gateway", " |= \"error\"", 100)
            .await
            .expect("log_tail should succeed");

        mock.assert_async().await;
        assert_eq!(result["status"], "200");
    }

    #[tokio::test]
    async fn log_tail_propagates_api_error() {
        let mut server = Server::new_async().await;
        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"/loki/api/v1/query_range".to_string()),
            )
            .with_status(500)
            .with_body("loki unavailable")
            .create_async()
            .await;

        let config = SreConfig::new("http://unused:3301", "http://unused:9090", server.url());
        let client = reqwest::Client::new();

        let err = log_tail(&client, &config, "api-gateway", "", 50)
            .await
            .expect_err("log_tail should propagate 500");

        assert!(
            matches!(err, SreError::ApiError { status: 500, .. }),
            "expected ApiError(500), got: {err}"
        );
    }
}
