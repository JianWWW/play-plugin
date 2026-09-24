//! Integration against a *real* FFmpeg stream. Ignored by default; run with:
//!
//!   PLAY_PLUGIN_TEST_URL=http://127.0.0.1:8090/test.flv \
//!     cargo test -p plugin-server --test real_stream -- --ignored --nocapture
//!
//! Generate + serve a test FLV locally:
//!   ffmpeg -f lavfi -i testsrc=size=640x360:rate=30 -t 30 \
//!     -c:v libx264 -pix_fmt yuv420p -f flv /tmp/test.flv
//!   node -e "…static server…"   (see docs/deployment.md)

use futures_util::{SinkExt, StreamExt};
use plugin_overlay::audio::AudioHub;
use plugin_overlay::host::OverlayHost;
use plugin_server::config::Config;
use plugin_server::session::{serve, AppState};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
#[ignore = "requires PLAY_PLUGIN_TEST_URL (real FFmpeg stream)"]
async fn real_stream_opens_and_plays() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_writer(std::io::stderr)
        .try_init();
    let url = std::env::var("PLAY_PLUGIN_TEST_URL").expect("PLAY_PLUGIN_TEST_URL not set");
    let config = Config {
        port: 0,
        port_fallback: 1,
        force_test_source: false,
        hardware_decode: true,
        ..Default::default()
    };
    let host = OverlayHost::start().expect("overlay host");
    let state = AppState::new(config.clone(), host, Arc::new(AudioHub::new()), "test");
    let port = serve(config, state).await.expect("serve");

    let mut request = format!("ws://127.0.0.1:{port}/ws")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Origin",
        axum::http::HeaderValue::from_static("https://test.local"),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let _ = ws.next().await; // server hello event

    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":1,"method":"hello","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let _ = next(&mut ws).await;

    ws.send(Message::Text(
        serde_json::json!({
            "v":1,"id":2,"method":"stream.open",
            "params":{"streamId":7,"url":url,"rect":{"l":100,"t":100,"w":640,"h":360},"muted":true}
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let resp = next(&mut ws).await;
    assert_eq!(resp["ok"], true, "open failed: {resp}");

    let mut codec = String::new();
    let mut decoder = String::new();
    let mut playing = false;
    let mut got_stats = false;
    for _ in 0..20 {
        let evt = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next()).await;
        let msg = match evt {
            Ok(Some(Ok(Message::Text(t)))) => t,
            _ => continue,
        };
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        match v["event"].as_str() {
            Some("stream.info") => {
                codec = v["params"]["codec"].as_str().unwrap_or("").into();
                decoder = v["params"]["decoder"].as_str().unwrap_or("").into();
                println!("INFO  {v}");
            }
            Some("stream.state") => {
                println!("STATE {v}");
                if v["params"]["state"] == "playing" {
                    playing = true;
                }
                if v["params"]["state"] == "error" {
                    panic!("stream error: {v}");
                }
            }
            Some("stream.stats") => {
                println!("STATS {v}");
                if v["params"]["fps"].as_f64().unwrap_or(0.0) > 0.0 {
                    got_stats = true;
                    break;
                }
            }
            _ => {}
        }
        if playing && got_stats {
            break;
        }
    }
    assert!(playing, "never reached playing state");
    assert!(!codec.is_empty(), "no stream.info");
    let _ = got_stats; // static test files EOF before the 1s stats window
    println!("codec={codec} decoder={decoder}");
    // Cleanup: idle timeout would reap anyway.
    ws.send(Message::Text(
        serde_json::json!({"v":1,"id":3,"method":"stream.close","params":{"streamId":7}})
            .to_string(),
    ))
    .await
    .unwrap();
}

async fn next(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        if let Message::Text(ref t) = msg {
            return serde_json::from_str(t).unwrap();
        }
    }
}
