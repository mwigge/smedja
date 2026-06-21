use crate::{SreConfig, SreError};

/// Queries the `SigNoz` traces API and returns the raw JSON response.
///
/// Sends a `GET` request to
/// `{otlp_endpoint}/api/v3/traces?service={service}&lookback={time_range_minutes}m`.
/// An optional `span_filter` (key=value string) is appended as-is when provided.
///
/// # Errors
///
/// - [`SreError::Http`] if the HTTP transport fails.
/// - [`SreError::ApiError`] if the server returns a non-2xx status code.
pub async fn otel_query(
    client: &reqwest::Client,
    config: &SreConfig,
    service: &str,
    span_filter: Option<&str>,
    time_range_minutes: u32,
) -> Result<serde_json::Value, SreError> {
    let mut url = format!(
        "{}/api/v3/traces?service={service}&lookback={time_range_minutes}m",
        config.otlp_endpoint,
    );
    if let Some(filter) = span_filter {
        url.push('&');
        url.push_str("filter=");
        url.push_str(filter);
    }

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
    async fn otel_query_sends_correct_request() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"/api/v3/traces".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": []}"#)
            .create_async()
            .await;

        let config = SreConfig::new(server.url(), "http://unused:9090", "http://unused:3100");
        let client = reqwest::Client::new();

        let result = otel_query(&client, &config, "my-svc", None, 15)
            .await
            .expect("otel_query should succeed");

        mock.assert_async().await;
        assert_eq!(result["data"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn otel_query_with_span_filter() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"/api/v3/traces\?.*filter=".to_string()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data": [{"traceId": "abc"}]}"#)
            .create_async()
            .await;

        let config = SreConfig::new(server.url(), "http://unused:9090", "http://unused:3100");
        let client = reqwest::Client::new();

        let result = otel_query(&client, &config, "my-svc", Some("span.kind=server"), 5)
            .await
            .expect("otel_query with filter should succeed");

        mock.assert_async().await;
        assert_eq!(result["data"][0]["traceId"], "abc");
    }

    #[tokio::test]
    async fn otel_query_propagates_api_error() {
        let mut server = Server::new_async().await;
        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"/api/v3/traces".to_string()),
            )
            .with_status(500)
            .with_body("internal server error")
            .create_async()
            .await;

        let config = SreConfig::new(server.url(), "http://unused:9090", "http://unused:3100");
        let client = reqwest::Client::new();

        let err = otel_query(&client, &config, "my-svc", None, 5)
            .await
            .expect_err("otel_query should propagate 500");

        assert!(
            matches!(err, SreError::ApiError { status: 500, .. }),
            "expected ApiError(500), got: {err}"
        );
    }
}
