//! AWS Bedrock provider — Converse API with `SigV4` signing.
//!
//! Reads credentials from standard AWS environment variables:
//! - `AWS_ACCESS_KEY_ID`
//! - `AWS_SECRET_ACCESS_KEY`
//! - `AWS_SESSION_TOKEN` (optional)
//! - `AWS_DEFAULT_REGION` (defaults to `us-east-1`)

// ---------------------------------------------------------------------------
// M19 — AWS Bedrock provider
// ---------------------------------------------------------------------------

use crate::{AdapterError, CallOptions, DeltaStream, Message, Provider};

/// Default AWS region when `AWS_DEFAULT_REGION` is not set.
pub const DEFAULT_REGION: &str = "us-east-1";

/// AWS credentials read from the environment.
#[derive(Clone)]
#[allow(dead_code)] // secret_access_key held for future SigV4 signing; not read until signing is wired in
pub struct AwsCredentials {
    pub access_key_id: String,
    // Private: never exposed directly — the manual Debug impl redacts it.
    secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[redacted]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl AwsCredentials {
    /// Loads credentials from standard AWS environment variables.
    ///
    /// Returns `None` when either `AWS_ACCESS_KEY_ID` or
    /// `AWS_SECRET_ACCESS_KEY` is absent.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok();
        Some(Self {
            access_key_id,
            secret_access_key,
            session_token,
        })
    }
}

/// A provider that routes requests through AWS Bedrock's Converse API.
pub struct BedrockProvider {
    /// AWS region (e.g. `us-east-1`).
    pub region: String,
    /// Bedrock model ID (e.g. `anthropic.claude-3-5-sonnet-20241022-v2:0`).
    pub model_id: String,
    credentials: AwsCredentials,
}

impl BedrockProvider {
    /// Creates a provider for `model_id` using credentials and region from the
    /// environment.
    ///
    /// Returns `None` when required credentials are absent.
    #[must_use]
    pub fn detect(model_id: impl Into<String>) -> Option<Self> {
        let credentials = AwsCredentials::from_env()?;
        let region =
            std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| DEFAULT_REGION.to_owned());
        Some(Self {
            region,
            model_id: model_id.into(),
            credentials,
        })
    }

    /// Creates a provider with explicit credentials and region.
    #[must_use]
    pub fn new(
        model_id: impl Into<String>,
        region: impl Into<String>,
        credentials: AwsCredentials,
    ) -> Self {
        Self {
            region: region.into(),
            model_id: model_id.into(),
            credentials,
        }
    }

    /// Returns the access key ID used for signing.
    #[must_use]
    pub fn access_key_id(&self) -> &str {
        &self.credentials.access_key_id
    }

    /// Builds the Bedrock Converse API request body from `messages`.
    ///
    /// Format: `{"messages": [{"role": "user"|"assistant", "content": [{"text": "..."}]}]}`
    #[must_use]
    pub fn build_converse_body(&self, messages: &[Message]) -> serde_json::Value {
        use crate::types::Role;
        let msgs: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                // System and Tool messages are mapped to "user" for Converse body.
                let role = match m.role {
                    Role::Assistant => "assistant",
                    Role::User | Role::System | Role::Tool => "user",
                };
                serde_json::json!({
                    "role": role,
                    "content": [{ "text": m.content }],
                })
            })
            .collect();
        serde_json::json!({ "messages": msgs })
    }
}

impl Provider for BedrockProvider {
    fn stream_chat(&self, _messages: &[Message], _opts: &CallOptions) -> DeltaStream {
        // ponytail: Bedrock Converse streaming requires SigV4 request signing;
        // returns an error stream until signing is wired in with the hmac crate.
        let err = AdapterError::Request("Bedrock provider requires SigV4 streaming (not yet implemented). Use ANTHROPIC_API_KEY or OPENAI_API_KEY instead.".into());
        Box::pin(tokio_stream::once(Err(err)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_ENV_LOCK as ENV_LOCK;

    #[test]
    fn bedrock_provider_region_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("AWS_DEFAULT_REGION").ok();
        std::env::set_var("AWS_DEFAULT_REGION", "eu-west-1");
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIATEST");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret");

        let provider = BedrockProvider::detect("anthropic.claude-3-5-sonnet-20241022-v2:0");

        std::env::remove_var("AWS_DEFAULT_REGION");
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        if let Some(v) = saved {
            std::env::set_var("AWS_DEFAULT_REGION", v);
        }

        let provider = provider.expect("should detect when credentials set");
        assert_eq!(provider.region, "eu-west-1");
    }

    #[test]
    fn bedrock_credentials_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIATESTKEY");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "mysecretkey");
        std::env::remove_var("AWS_SESSION_TOKEN");

        let creds = AwsCredentials::from_env();

        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");

        let creds = creds.expect("credentials must load from env");
        assert_eq!(creds.access_key_id, "AKIATESTKEY");
        // secret_access_key is private; verify it loaded by checking the Debug
        // representation redacts it (proving the manual Debug impl runs).
        let debug_str = format!("{creds:?}");
        assert!(debug_str.contains("secret_access_key: \"[redacted]\""));
        assert!(creds.session_token.is_none());
    }

    #[test]
    fn bedrock_converse_request_format() {
        use crate::types::Role;

        let creds = AwsCredentials {
            access_key_id: "AKIATEST".into(),
            secret_access_key: "secret".into(),
            session_token: None,
        };
        let provider = BedrockProvider::new(
            "anthropic.claude-3-5-sonnet-20241022-v2:0",
            "us-east-1",
            creds,
        );
        let messages = vec![Message {
            role: Role::User,
            content: "Hello, world!".into(),
        }];
        let body = provider.build_converse_body(&messages);
        let msgs = body["messages"]
            .as_array()
            .expect("must have messages array");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "user");
        assert_eq!(
            msgs[0]["content"][0]["text"].as_str().unwrap(),
            "Hello, world!"
        );
    }
}
