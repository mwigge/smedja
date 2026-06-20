use std::path::PathBuf;

use serde_json::json;
use tokio::net::UnixListener;

use smedja_rpc::{client::Client, router::Router, server::Server};

fn temp_sock() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("smedja-test-{}.sock", uuid::Uuid::new_v4()));
    p
}

fn start_server(router: Router) -> PathBuf {
    let path = temp_sock();
    let listener = UnixListener::bind(&path).unwrap();
    let server = Server::new(router);
    tokio::spawn(async move { server.serve(listener).await });
    path
}

#[tokio::test]
async fn client_calls_registered_method() {
    let mut router = Router::new();
    router.register("ping", |_| async { Ok(json!("pong")) });

    let path = start_server(router);
    let mut client = Client::connect(&path).await.unwrap();

    let result = client.call("ping", json!({})).await.unwrap();
    assert_eq!(result, json!("pong"));
}

#[tokio::test]
async fn client_receives_rpc_error_for_unknown_method() {
    let router = Router::new();
    let path = start_server(router);
    let mut client = Client::connect(&path).await.unwrap();

    let err = client.call("nope", json!({})).await.unwrap_err();
    assert_eq!(err.code, smedja_rpc::codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn client_receives_handler_params() {
    let mut router = Router::new();
    router.register("add", |params| async move {
        let a = params["a"].as_i64().unwrap_or(0);
        let b = params["b"].as_i64().unwrap_or(0);
        Ok(json!(a + b))
    });

    let path = start_server(router);
    let mut client = Client::connect(&path).await.unwrap();

    let result = client.call("add", json!({"a": 3, "b": 4})).await.unwrap();
    assert_eq!(result, json!(7));
}

#[tokio::test]
async fn multiple_sequential_calls_use_incrementing_ids() {
    let mut router = Router::new();
    router.register("echo", |params| async move { Ok(params) });

    let path = start_server(router);
    let mut client = Client::connect(&path).await.unwrap();

    for i in 0..5_i64 {
        let result = client.call("echo", json!({"n": i})).await.unwrap();
        assert_eq!(result["n"], i);
    }
}

#[tokio::test]
async fn notification_does_not_block_client() {
    let mut router = Router::new();
    router.register("ping", |_| async { Ok(json!("pong")) });

    let path = start_server(router);
    let mut client = Client::connect(&path).await.unwrap();

    // Notification: fire and forget — should not block or error
    client.notify("session.end", json!({})).await.unwrap();

    // Subsequent call still works
    let result = client.call("ping", json!({})).await.unwrap();
    assert_eq!(result, json!("pong"));
}

#[tokio::test]
async fn multiple_concurrent_clients() {
    let mut router = Router::new();
    router.register("ping", |_| async { Ok(json!("pong")) });

    let path = start_server(router);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let p = path.clone();
            tokio::spawn(async move {
                let mut client = Client::connect(&p).await.unwrap();
                client.call("ping", json!({})).await.unwrap()
            })
        })
        .collect();

    for h in handles {
        assert_eq!(h.await.unwrap(), json!("pong"));
    }
}
