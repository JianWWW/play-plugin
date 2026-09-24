//! End-to-end: bind server → WS client (browser-simulating) → hello →
//! open test stream → state/info events → rect/mute/status → close.
//! Uses `test://` sources so no network or real encoder is required.

use axum::http::HeaderValue;
use futures_util::{SinkExt, StreamExt};
use plugin_overlay::audio::AudioHub;
use plugin_overlay::host::OverlayHost;
use plugin_server::config::Config;
use plugin_server::session::{serve, AppState};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

async fn start_server(origins: Vec<String>) -> (Arc<AppState>, u16) {
    let config = Config {
        port: 0,
        port_fallback: 1,
        origins,
        force_test_source: true,
        ..Default::default()
    };
    let host = OverlayHost::start().expect("overlay host");
    let audio = std::sync::Arc::new(AudioHub::new());
    let state = AppState::new(config.clone(), host, audio, "0.1.0-test");
    let port = serve(config, state.clone()).await.expect("serve");
    (state, port)
}

fn ws_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}/ws")
}

async fn connect(
    port: u16,
    origin: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = ws_url(port).into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_str(origin).unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("connect");
    ws
}

#[tokio::test]
async fn full_session_flow() {
    let (_state, port) = start_server(vec!["https://player.example.com".into()]).await;

    let mut ws = connect(port, "https://player.example.com").await;

    // Server hello event arrives first.
    let first = ws.next().await.unwrap().unwrap();
    let hello_evt: serde_json::Value = match first {
        Message::Text(ref t) => serde_json::from_str(t).unwrap(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(hello_evt["event"], "hello");
    assert_eq!(hello_evt["params"]["protocol"], 1);

    // hello request → capabilities.
    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":1,"method":"hello","params":{"client":"e2e","protocol":1}})
            .to_string(),
    ))
    .await
    .unwrap();
    let resp = next_text(&mut ws).await;
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["result"]["hello"]["protocol"], 1);
    assert_eq!(resp["result"]["hello"]["capabilities"]["h265"], true);

    // Method before hello is rejected — new connection.
    drop(ws);
    let mut ws2 = connect(port, "https://player.example.com").await;
    let _ = ws2.next().await; // server hello event
    ws2.send(Message::Text(
        serde_json::json!({"v":1,"id":9,"method":"app.status","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let resp = next_text(&mut ws2).await;
    assert_eq!(resp["error"]["code"], "NOT_AUTHENTICATED");
    drop(ws2);

    // Open a test stream on the authenticated connection.
    let mut ws = connect(port, "https://player.example.com").await;
    let _ = ws.next().await; // server hello
    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":1,"method":"hello","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let _ = next_text(&mut ws).await;

    ws.send(Message::Text(
        serde_json::json!({
            "v":1,"id":2,"method":"stream.open",
            "params":{"streamId":42,"url":"test://pattern?w=320&h=180&fps=30",
                      "rect":{"l":10,"t":20,"w":320,"h":180},"muted":true}
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let resp = next_text(&mut ws).await;
    assert_eq!(resp["id"], 2);
    assert_eq!(resp["ok"], true);

    // Expect stream.info + stream.state(playing) events.
    let mut got_info = false;
    let mut got_playing = false;
    for _ in 0..4 {
        let evt = next_text(&mut ws).await;
        match evt["event"].as_str() {
            Some("stream.info") => {
                got_info = true;
                assert_eq!(evt["params"]["streamId"], 42);
                assert_eq!(evt["params"]["decoder"], "test");
            }
            Some("stream.state") if evt["params"]["state"] == "playing" => got_playing = true,
            _ => {}
        }
        if got_info && got_playing {
            break;
        }
    }
    assert!(got_info, "missing stream.info");
    assert!(got_playing, "missing playing state");

    // Duplicate streamId rejected.
    ws.send(Message::Text(
        serde_json::json!({
            "v":1,"id":3,"method":"stream.open",
            "params":{"streamId":42,"url":"test://pattern","rect":{"l":0,"t":0,"w":10,"h":10},"muted":true}
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let resp = next_text(&mut ws).await;
    assert_eq!(resp["error"]["code"], "ALREADY_EXISTS");

    // rect + mute + status.
    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":4,"method":"stream.rect","params":{"streamId":42,"rect":{"l":0,"t":0,"w":640,"h":360},"hidden":false}}).to_string(),
    ))
    .await
    .unwrap();
    let resp = next_text(&mut ws).await;
    assert_eq!(resp["ok"], true);

    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":5,"method":"app.status","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let resp = next_text(&mut ws).await;
    assert_eq!(resp["result"]["status"]["activeStreams"], 1);

    // stats event should arrive within ~2s (test source emits every second).
    let mut got_stats = false;
    for _ in 0..6 {
        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await;
        let evt = match evt {
            Ok(Some(Ok(Message::Text(t)))) => {
                serde_json::from_str::<serde_json::Value>(&t).unwrap()
            }
            Ok(None) => panic!("connection closed"),
            Err(_) => break,
            _ => continue,
        };
        if evt["event"] == "stream.stats" {
            got_stats = true;
            assert!(evt["params"]["fps"].as_f64().unwrap() > 0.0);
            break;
        }
    }
    assert!(got_stats, "missing stream.stats");

    // close stream.
    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":6,"method":"stream.close","params":{"streamId":42}})
            .to_string(),
    ))
    .await
    .unwrap();
    let resp = next_text(&mut ws).await;
    assert_eq!(resp["ok"], true);
}

#[tokio::test]
async fn rejects_evil_origin() {
    let (_state, port) = start_server(vec!["https://player.example.com".into()]).await;
    let mut request = ws_url(port).into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static("https://evil.com"));
    let result = tokio_tungstenite::connect_async(request).await;
    assert!(result.is_err(), "evil origin must be rejected");
}

#[tokio::test]
async fn unknown_method_gets_error() {
    let (_state, port) = start_server(vec![]).await; // dev mode: allow all
    let mut ws = connect(port, "https://anything.local").await;
    let _ = ws.next().await;
    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":1,"method":"hello","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let _ = next_text(&mut ws).await;
    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":2,"method":"hack.system","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let resp = next_text(&mut ws).await;
    assert_eq!(resp["error"]["code"], "UNKNOWN_METHOD");
}

async fn next_text(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("timeout waiting for message")
            .expect("connection closed")
            .expect("ws error");
        // Skip protocol-level pong frames.
        if let Message::Text(ref t) = msg {
            return serde_json::from_str(t).unwrap();
        }
        if let Message::Close(ref c) = msg {
            panic!("unexpected close: {c:?}");
        }
    }
}
