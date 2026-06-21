use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

pub struct LocalHealth {
    pub healthy: bool,
    pub tool_format: String, // "openai" or "xml"
    pub last_checked: f64,   // epoch seconds (use std::time)
    pub model: Option<String>,
}

impl Default for LocalHealth {
    fn default() -> Self {
        Self {
            healthy: false,
            tool_format: "openai".to_owned(),
            last_checked: 0.0,
            model: None,
        }
    }
}

static HEALTH_CACHE: OnceLock<Arc<Mutex<LocalHealth>>> = OnceLock::new();

pub fn global_health() -> Arc<Mutex<LocalHealth>> {
    Arc::clone(HEALTH_CACHE.get_or_init(|| Arc::new(Mutex::new(LocalHealth::default()))))
}

pub async fn check_health(base_url: &str) -> LocalHealth {
    // Task 33: env override skips HTTP fetch
    if let Ok(fmt) = std::env::var("SMEDJA_LOCAL_TOOL_FORMAT") {
        tracing::info!(tool_format = %fmt, "SMEDJA_LOCAL_TOOL_FORMAT override active; skipping health fetch");
        let now = now_epoch();
        return LocalHealth {
            healthy: true,
            tool_format: fmt,
            last_checked: now,
            model: None,
        };
    }

    let url = format!("{base_url}/v1/models");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "local provider unreachable");
            return LocalHealth {
                last_checked: now_epoch(),
                ..LocalHealth::default()
            };
        }
    };

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            let tool_format = body
                .pointer("/data/0/capabilities/tool_format")
                .and_then(|v| v.as_str())
                .unwrap_or("openai")
                .to_owned();
            let model = body
                .pointer("/data/0/id")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let health = LocalHealth {
                healthy: true,
                tool_format,
                last_checked: now_epoch(),
                model: model.clone(),
            };
            tracing::info!(healthy = true, model = ?model, "local health check");
            health
        }
        Ok(resp) => {
            let status = resp.status();
            tracing::warn!(status = %status, "local provider unreachable");
            LocalHealth {
                last_checked: now_epoch(),
                ..LocalHealth::default()
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "local provider unreachable");
            LocalHealth {
                last_checked: now_epoch(),
                ..LocalHealth::default()
            }
        }
    }
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// Task 35: unit tests
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn env_override_skips_http() {
        // SAFETY: single-threaded test; set then clear env var
        unsafe { std::env::set_var("SMEDJA_LOCAL_TOOL_FORMAT", "xml") };
        let h = check_health("http://127.0.0.1:19999").await; // port that won't be listening
        unsafe { std::env::remove_var("SMEDJA_LOCAL_TOOL_FORMAT") };
        assert!(h.healthy);
        assert_eq!(h.tool_format, "xml");
    }

    #[tokio::test]
    async fn unhealthy_when_no_server() {
        // Ensure override is not set
        unsafe { std::env::remove_var("SMEDJA_LOCAL_TOOL_FORMAT") };
        let h = check_health("http://127.0.0.1:19998").await;
        assert!(!h.healthy);
    }

    #[tokio::test]
    async fn healthy_mock_server() {
        // Spawn a minimal axum server returning a models response
        use axum::{routing::get, Router};
        use std::net::SocketAddr;

        let app = Router::new().route(
            "/v1/models",
            get(|| async {
                axum::response::Json(serde_json::json!({
                    "data": [{ "id": "qwen3", "capabilities": { "tool_format": "xml" } }]
                }))
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        unsafe { std::env::remove_var("SMEDJA_LOCAL_TOOL_FORMAT") };
        let base = format!("http://{addr}");
        let h = check_health(&base).await;
        assert!(h.healthy);
        assert_eq!(h.tool_format, "xml");
        assert_eq!(h.model.as_deref(), Some("qwen3"));
    }
}
