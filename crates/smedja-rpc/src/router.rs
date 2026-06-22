use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::{codes, RpcError};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type HandlerFn = Arc<dyn Fn(Value) -> BoxFuture<Result<Value, RpcError>> + Send + Sync + 'static>;

/// Async method dispatcher. Build with `register`, then wrap in `Server`.
#[derive(Default)]
pub struct Router {
    handlers: HashMap<String, HandlerFn>,
}

impl Router {
    /// Creates a new empty router with no registered handlers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an async handler for `method`.
    pub fn register<F, Fut>(&mut self, method: impl Into<String>, handler: F) -> &mut Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, RpcError>> + Send + 'static,
    {
        self.handlers
            .insert(method.into(), Arc::new(move |p| Box::pin(handler(p))));
        self
    }

    /// Dispatch a method call and return the handler's result.
    ///
    /// # Errors
    /// Returns `METHOD_NOT_FOUND` if no handler is registered for `method`,
    /// or the handler's own error if it fails.
    #[must_use = "check the Result; unhandled dispatch errors silently drop responses"]
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match self.handlers.get(method) {
            Some(h) => h(params).await,
            None => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn dispatch_calls_registered_handler() {
        let mut router = Router::new();
        router.register("ping", |_| async { Ok(json!("pong")) });

        let result = router.dispatch("ping", json!({})).await.unwrap();
        assert_eq!(result, json!("pong"));
    }

    #[tokio::test]
    async fn dispatch_passes_params_to_handler() {
        let mut router = Router::new();
        router.register("echo", |params| async move { Ok(params) });

        let result = router
            .dispatch("echo", json!({"msg": "hello"}))
            .await
            .unwrap();
        assert_eq!(result["msg"], "hello");
    }

    #[tokio::test]
    async fn dispatch_unknown_method_returns_method_not_found() {
        let router = Router::new();
        let err = router.dispatch("nope", json!({})).await.unwrap_err();
        assert_eq!(err.code, crate::codes::METHOD_NOT_FOUND);
        assert!(err.message.contains("nope"));
    }

    #[tokio::test]
    async fn dispatch_handler_error_propagates() {
        let mut router = Router::new();
        router.register("fail", |_| async {
            Err(RpcError::new(-32602, "bad params"))
        });

        let err = router.dispatch("fail", json!({})).await.unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[tokio::test]
    async fn multiple_methods_dispatch_independently() {
        let mut router = Router::new();
        router.register("a", |_| async { Ok(json!("A")) });
        router.register("b", |_| async { Ok(json!("B")) });

        assert_eq!(router.dispatch("a", json!({})).await.unwrap(), json!("A"));
        assert_eq!(router.dispatch("b", json!({})).await.unwrap(), json!("B"));
    }
}
