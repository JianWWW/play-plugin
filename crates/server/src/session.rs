//! WebSocket sessions, stream registry, event routing.

use crate::config::Config;
use crate::protocol::*;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use plugin_core::{PipelineEvent, PipelineHandle, StreamOptions, StreamState};
use plugin_overlay::audio::AudioHub;
use plugin_overlay::host::{OverlayHost, Rect};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub const APP_NAME: &str = "PlayPlugin";
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

pub struct StreamEntry {
    pub conn_id: u64,
    pub handle: PipelineHandle,
    pub decoder: String,
    pub state: StreamState,
    pub last_fps: f32,
    pub last_bitrate_kbps: f64,
    pub url: String, // already redacted
}

pub struct AppState {
    pub config: Config,
    pub host: OverlayHost,
    pub audio: Arc<AudioHub>,
    pub version: String,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    streams: HashMap<u64, StreamEntry>,
    conns: HashMap<u64, mpsc::UnboundedSender<Message>>,
    next_conn_id: u64,
}

impl AppState {
    pub fn new(
        config: Config,
        host: OverlayHost,
        audio: Arc<AudioHub>,
        version: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            host,
            audio,
            version: version.into(),
            inner: Mutex::new(Inner::default()),
        })
    }

    pub fn active_streams(&self) -> u32 {
        self.inner.lock().unwrap().streams.len() as u32
    }

    pub fn broadcast(&self, event: &Event) {
        let text = serde_json::to_string(event).unwrap_or_default();
        let mut inner = self.inner.lock().unwrap();
        inner
            .conns
            .retain(|_, tx| tx.send(Message::Text(text.clone())).is_ok());
    }

    /// Single click on an overlay: notify the owning page so it can mark the
    /// stream as selected.
    pub fn notify_select(&self, stream_id: u64) {
        let event = Event::new(
            "stream.select",
            serde_json::json!({ "streamId": stream_id }),
        );
        let text = serde_json::to_string(&event).unwrap_or_default();
        let inner = self.inner.lock().unwrap();
        if let Some(e) = inner.streams.get(&stream_id) {
            if let Some(tx) = inner.conns.get(&e.conn_id) {
                let _ = tx.send(Message::Text(text));
            }
        }
    }

    /// Native control-bar button press: routed to the owning page as
    /// `app.control` (the SDK applies defaults or hands it to `onControl`).
    pub fn notify_control(&self, stream_id: u64, button: String, value: Option<f64>) {
        let event = Event::control(crate::protocol::ControlEvent {
            stream_id,
            button,
            value,
        });
        let text = serde_json::to_string(&event).unwrap_or_default();
        let inner = self.inner.lock().unwrap();
        if let Some(e) = inner.streams.get(&stream_id) {
            if let Some(tx) = inner.conns.get(&e.conn_id) {
                let _ = tx.send(Message::Text(text));
            }
        }
    }

    fn register_conn(&self, tx: mpsc::UnboundedSender<Message>) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_conn_id;
        inner.next_conn_id += 1;
        inner.conns.insert(id, tx);
        id
    }

    fn remove_conn(&self, conn_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.conns.remove(&conn_id);
        let mine: Vec<u64> = inner
            .streams
            .iter()
            .filter(|(_, e)| e.conn_id == conn_id)
            .map(|(id, _)| *id)
            .collect();
        for id in mine {
            if let Some(e) = inner.streams.remove(&id) {
                e.handle
                    .close
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.audio.drop_stream(id);
                self.host.close(id);
                info!(stream_id = id, "stream reaped (connection gone)");
            }
        }
    }
}

/// Binds 127.0.0.1 (with port fallback) and serves /info + /ws.
pub async fn serve(config: Config, state: Arc<AppState>) -> anyhow::Result<u16> {
    let mut bound = None;
    for p in config.port..config.port.saturating_add(config.port_fallback) {
        match tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
            Ok(l) => {
                bound = Some(l);
                break;
            }
            Err(e) => debug!(port = p, error = %e, "port busy"),
        }
    }
    let listener = bound.ok_or_else(|| anyhow::anyhow!("no free port in range"))?;
    let port = listener.local_addr()?.port();

    let app = axum::Router::new()
        .route(
            "/info",
            axum::routing::get(info_handler).options(preflight_handler),
        )
        .route(
            "/ws",
            axum::routing::get(ws_upgrade).options(preflight_handler),
        )
        .fallback(preflight_handler)
        .with_state(state.clone());

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "http server stopped");
        }
    });

    // Overlay UI events: single-click select and control-bar button presses.
    // Fullscreen toggling is handled entirely inside the overlay host.
    let ui_state = state.clone();
    tokio::spawn(async move {
        loop {
            match ui_state.host.try_recv_event() {
                Some(plugin_overlay::host::OverlayUiEvent::Select { stream_id }) => {
                    ui_state.notify_select(stream_id);
                }
                Some(plugin_overlay::host::OverlayUiEvent::Control {
                    stream_id,
                    button,
                    value,
                }) => {
                    ui_state.notify_control(stream_id, button.to_string(), value);
                }
                None => {}
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    info!(port, "listening on ws://127.0.0.1:{port}/ws");
    Ok(port)
}

async fn info_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // /info only reveals version info; CORS + PNA headers are required so the
    // browser fetch() from any https page can read it.
    let mut resp = axum::Json(serde_json::json!({
        "plugin": APP_NAME,
        "version": state.version,
        "protocol": PROTOCOL_VERSION,
    }))
    .into_response();
    for (k, v) in cors_headers("*") {
        resp.headers_mut().insert(k, v);
    }
    resp
}

async fn preflight_handler() -> axum::response::Response {
    use axum::http::{header, HeaderValue};
    use axum::response::IntoResponse;
    let mut resp = axum::http::StatusCode::NO_CONTENT.into_response();
    let headers = [
        (
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, OPTIONS"),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type"),
        ),
        (
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("86400"),
        ),
    ];
    for (k, v) in headers {
        resp.headers_mut().insert(k, v);
    }
    // Chrome Private Network Access / Local Network Access preflights.
    let pna = axum::http::HeaderName::from_static("access-control-allow-private-network");
    let lna = axum::http::HeaderName::from_static("access-control-allow-local-network");
    if let Ok(v) = HeaderValue::from_str("true") {
        resp.headers_mut().insert(pna, v.clone());
        resp.headers_mut().insert(lna, v);
    }
    resp
}

fn cors_headers(origin: &str) -> Vec<(axum::http::HeaderName, axum::http::HeaderValue)> {
    use axum::http::header;
    vec![
        (
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            axum::http::HeaderValue::from_str(origin)
                .unwrap_or(axum::http::HeaderValue::from_static("*")),
        ),
        (
            axum::http::HeaderName::from_static("access-control-allow-private-network"),
            axum::http::HeaderValue::from_static("true"),
        ),
    ]
}

async fn ws_upgrade(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !state.config.origin_allowed(&origin) {
        warn!(%origin, "origin rejected");
        use axum::response::IntoResponse;
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    if state.config.origins.is_empty() {
        debug!("origin whitelist empty — dev mode, allowing {}", origin);
    }
    upgrade.on_upgrade(move |socket| handle_socket(socket, state, origin))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, origin: String) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let conn_id = state.register_conn(tx.clone());
    info!(conn_id, %origin, "client connected");
    let _ = tx.send(Message::Text(
        serde_json::to_string(&Event::new(
            "hello",
            serde_json::json!({ "plugin": APP_NAME, "version": state.version, "protocol": PROTOCOL_VERSION }),
        ))
        .unwrap_or_default(),
    ));

    let (mut sender, mut receiver) = socket.split();

    // Writer task.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Heartbeat ticker: protocol-level pings; browsers auto-pong, which we
    // count as activity (protects against half-open TCP).
    let hb_tx = tx.clone();
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            tick.tick().await;
            if hb_tx.send(Message::Ping(Vec::new())).is_err() {
                break;
            }
        }
    });

    let authenticated = Arc::new(AtomicBool::new(false));
    let last_activity = Arc::new(AtomicU64::new(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    ));

    let mut dead = false;
    while !dead {
        let idle = if authenticated.load(std::sync::atomic::Ordering::Relaxed) {
            IDLE_TIMEOUT
        } else {
            HELLO_TIMEOUT
        };
        match tokio::time::timeout(idle, receiver.next()).await {
            Ok(Some(Ok(msg))) => {
                stamp(&last_activity);
                match msg {
                    Message::Text(text) => {
                        if let Err(e) = handle_text(&state, conn_id, &tx, &text, &authenticated) {
                            debug!(conn_id, error = %e, "request failed");
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => dead = true,
                    _ => {}
                }
            }
            Ok(Some(Err(e))) => {
                debug!(conn_id, error = %e, "ws error");
                dead = true;
            }
            Ok(None) => dead = true,
            Err(_) => {
                // Timeout: check wall-clock activity (covers non-pong clients).
                let last = last_activity.load(std::sync::atomic::Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                if now.saturating_sub(last) > idle.as_millis() as u64 {
                    info!(conn_id, "client idle timeout, reaping streams");
                    dead = true;
                }
            }
        }
    }

    heartbeat.abort();
    writer.abort();
    state.remove_conn(conn_id);
    info!(conn_id, "client disconnected");
}

fn stamp(last: &AtomicU64) {
    last.store(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

fn handle_text(
    state: &Arc<AppState>,
    conn_id: u64,
    tx: &mpsc::UnboundedSender<Message>,
    text: &str,
    authenticated: &AtomicBool,
) -> anyhow::Result<()> {
    let req: Request =
        serde_json::from_str(text).map_err(|e| anyhow::anyhow!("bad request: {e}"))?;
    let id = req.id;

    if req.method == "hello" {
        // Page-requested debug: raise the process log level (raise-only, ADR-0001).
        if req.params.get("debug").and_then(|v| v.as_bool()).unwrap_or(false) {
            info!(conn_id, "client requested debug logging, raising log level");
            crate::logging::raise_to("debug");
        }
        authenticated.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(Message::Text(ok_response(
            id,
            Result_ {
                hello: Some(HelloResult {
                    plugin: APP_NAME.into(),
                    version: state.version.clone(),
                    protocol: PROTOCOL_VERSION,
                    max_streams: state.config.max_streams,
                    capabilities: Capabilities {
                        h264: true,
                        h265: true,
                        rtsp: true,
                        flv: true,
                        hls: true,
                        rtmp: true,
                        udp: true,
                        hardware_decode: state.config.hardware_decode,
                    },
                }),
                ..Default::default()
            },
        )));
        return Ok(());
    }

    if !authenticated.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = tx.send(Message::Text(err_response(
            id,
            ProtocolError::new("NOT_AUTHENTICATED", "send hello first"),
        )));
        return Ok(());
    }

    let reply = dispatch(state, conn_id, &req);
    let _ = tx.send(Message::Text(reply));
    Ok(())
}

fn dispatch(state: &Arc<AppState>, conn_id: u64, req: &Request) -> String {
    match req.method.as_str() {
        "stream.open" => match serde_json::from_value::<OpenParams>(req.params.clone()) {
            Ok(p) => open_stream(state, conn_id, req.id, p),
            Err(e) => err_response(req.id, ProtocolError::new("BAD_PARAMS", e.to_string())),
        },
        "stream.rect" => match serde_json::from_value::<RectParams>(req.params.clone()) {
            Ok(p) => {
                state.host.set_rect(
                    p.stream_id,
                    Rect {
                        l: p.rect.l,
                        t: p.rect.t,
                        w: p.rect.w,
                        h: p.rect.h,
                    },
                    p.hidden,
                );
                ok_response(req.id, Result_::default())
            }
            Err(e) => err_response(req.id, ProtocolError::new("BAD_PARAMS", e.to_string())),
        },
        "stream.mute" => match serde_json::from_value::<MuteParams>(req.params.clone()) {
            Ok(p) => {
                let inner = state.inner.lock().unwrap();
                if let Some(e) = inner.streams.get(&p.stream_id) {
                    e.handle
                        .muted
                        .store(p.muted, std::sync::atomic::Ordering::Relaxed);
                    if p.muted {
                        state.audio.drop_stream(p.stream_id);
                    }
                    ok_response(req.id, Result_::default())
                } else {
                    err_response(req.id, ProtocolError::new("NOT_FOUND", "unknown streamId"))
                }
            }
            Err(e) => err_response(req.id, ProtocolError::new("BAD_PARAMS", e.to_string())),
        },
        "stream.snapshot" => match serde_json::from_value::<SnapshotParams>(req.params.clone()) {
            Ok(p) => snapshot_stream(state, req.id, p.stream_id),
            Err(e) => err_response(req.id, ProtocolError::new("BAD_PARAMS", e.to_string())),
        },
        "stream.pause" => match serde_json::from_value::<PauseParams>(req.params.clone()) {
            Ok(p) => pause_stream(state, req.id, p),
            Err(e) => err_response(req.id, ProtocolError::new("BAD_PARAMS", e.to_string())),
        },
        "stream.seek" => match serde_json::from_value::<SeekParams>(req.params.clone()) {
            Ok(p) if p.time_ms >= 0 => seek_stream(state, req.id, p),
            Ok(_) => err_response(
                req.id,
                ProtocolError::new("BAD_PARAMS", "timeMs must be >= 0"),
            ),
            Err(e) => err_response(req.id, ProtocolError::new("BAD_PARAMS", e.to_string())),
        },
        "stream.setSpeed" => match serde_json::from_value::<SpeedParams>(req.params.clone()) {
            Ok(p) if p.speed > 0.0 && p.speed <= 64.0 => set_speed(state, req.id, p),
            Ok(_) => err_response(
                req.id,
                ProtocolError::new("BAD_PARAMS", "speed must be in (0, 64]"),
            ),
            Err(e) => err_response(req.id, ProtocolError::new("BAD_PARAMS", e.to_string())),
        },
        "stream.record.start" => {
            record_stream(state, req.id, req.params.clone(), true)
        }
        "stream.record.stop" => record_stream(state, req.id, req.params.clone(), false),
        "stream.close" => match serde_json::from_value::<SnapshotParams>(req.params.clone()) {
            Ok(p) => {
                let entry = state.inner.lock().unwrap().streams.remove(&p.stream_id);
                if let Some(e) = entry {
                    if e.conn_id == conn_id {
                        e.handle
                            .close
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        state.audio.drop_stream(p.stream_id);
                        state.host.close(p.stream_id);
                    }
                    ok_response(req.id, Result_::default())
                } else {
                    err_response(req.id, ProtocolError::new("NOT_FOUND", "unknown streamId"))
                }
            }
            Err(e) => err_response(req.id, ProtocolError::new("BAD_PARAMS", e.to_string())),
        },
        "app.status" => {
            let active = state.active_streams();
            ok_response(
                req.id,
                Result_ {
                    status: Some(StatusResult {
                        version: state.version.clone(),
                        active_streams: active,
                    }),
                    ..Default::default()
                },
            )
        }
        "ping" => {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            ok_response(
                req.id,
                Result_ {
                    pong: Some(ts),
                    ..Default::default()
                },
            )
        }
        other => err_response(
            req.id,
            ProtocolError::new("UNKNOWN_METHOD", format!("unknown method '{other}'")),
        ),
    }
}

fn open_stream(state: &Arc<AppState>, conn_id: u64, id: u64, p: OpenParams) -> String {
    if !is_supported_url(&p.url) {
        return err_response(
            id,
            ProtocolError::new(
                "BAD_URL",
                "unsupported scheme (rtsp/rtsps/rtmp/rtmps/http/https/udp/test only)",
            ),
        );
    }
    if let Err(e) = validate_controls(p.controls.as_deref().unwrap_or(&[])) {
        return err_response(id, ProtocolError::new("BAD_PARAMS", e));
    }
    let mut inner = state.inner.lock().unwrap();
    if inner.streams.contains_key(&p.stream_id) {
        return err_response(
            id,
            ProtocolError::new("ALREADY_EXISTS", "streamId already open"),
        );
    }
    if inner.streams.len() >= state.config.max_streams as usize {
        return err_response(
            id,
            ProtocolError::new(
                "LIMIT_STREAMS",
                format!("max {} streams", state.config.max_streams),
            ),
        );
    }

    let opts = StreamOptions {
        url: p.url.clone(),
        muted: p.muted,
        title: p.title.clone().unwrap_or_default(),
    };
    let (etx, erx) = mpsc::unbounded_channel::<PipelineEvent>();
    let device = if state.config.hardware_decode {
        Some(state.host.gpu())
    } else {
        None
    };
    let handle = plugin_core::spawn_stream(opts, device, state.config.force_test_source, etx);

    let rect = p.rect;
    let ui = plugin_overlay::host::UiConfig {
        title: p.title.clone().unwrap_or_default(),
        buttons: p.controls.clone().unwrap_or_else(plugin_overlay::host::default_buttons),
        speeds: p
            .speeds
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| vec![1.0, 2.0, 4.0, 8.0, 16.0]),
    };
    state.host.create(
        p.stream_id,
        Rect {
            l: rect.l,
            t: rect.t,
            w: rect.w,
            h: rect.h,
        },
        ui,
    );

    inner.streams.insert(
        p.stream_id,
        StreamEntry {
            conn_id,
            handle,
            decoder: "pending".into(),
            state: StreamState::Connecting,
            last_fps: 0.0,
            last_bitrate_kbps: 0.0,
            url: crate::redact_url(&p.url),
        },
    );
    drop(inner);

    // Event pump: pipeline → renderer/audio + WebSocket events.
    let st = state.clone();
    let sid = p.stream_id;
    tokio::spawn(async move {
        let mut erx = erx;
        while let Some(ev) = erx.recv().await {
            match &ev {
                PipelineEvent::Video(frame) => st.host.set_frame(sid, frame.clone()),
                PipelineEvent::Audio(chunk) => {
                    if !st
                        .inner
                        .lock()
                        .unwrap()
                        .streams
                        .get(&sid)
                        .map(|e| !e.handle.muted.load(std::sync::atomic::Ordering::Relaxed))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    st.audio.push(sid, chunk);
                }
                PipelineEvent::Info {
                    codec,
                    width,
                    height,
                    decoder,
                } => {
                    if let Some(e) = st.inner.lock().unwrap().streams.get_mut(&sid) {
                        e.decoder = decoder.clone();
                    }
                    st.broadcast(&Event::stream_info(StreamInfoEvent {
                        stream_id: sid,
                        codec: codec.clone(),
                        width: *width,
                        height: *height,
                        decoder: decoder.clone(),
                    }));
                }
                PipelineEvent::Stats { fps, bitrate_kbps } => {
                    let dropped = st.host.dropped_frames(sid);
                    let decoder = st
                        .inner
                        .lock()
                        .unwrap()
                        .streams
                        .get_mut(&sid)
                        .map(|e| {
                            e.last_fps = *fps;
                            e.last_bitrate_kbps = *bitrate_kbps;
                            e.decoder.clone()
                        })
                        .unwrap_or_default();
                    st.broadcast(&Event::stream_stats(StreamStatsEvent {
                        stream_id: sid,
                        fps: *fps,
                        bitrate_kbps: *bitrate_kbps,
                        dropped,
                        decoder,
                    }));
                }
                PipelineEvent::State {
                    state: s,
                    code,
                    message,
                } => {
                    if let Some(e) = st.inner.lock().unwrap().streams.get_mut(&sid) {
                        e.state = *s;
                    }
                    st.broadcast(&Event::stream_state(StreamStateEvent {
                        stream_id: sid,
                        state: *s,
                        code: *code,
                        message: message.clone(),
                    }));
                }
            }
        }
    });

    info!(stream_id = p.stream_id, url = %crate::redact_url(&p.url), "stream open requested");
    ok_response(id, Result_::default())
}

/// Display freeze: keep draining the stream, skip decode/render. The page
/// learns via the broadcast `stream.state` (paused/playing).
fn pause_stream(state: &Arc<AppState>, id: u64, p: PauseParams) -> String {
    let new_state = {
        let mut inner = state.inner.lock().unwrap();
        match inner.streams.get_mut(&p.stream_id) {
            None => {
                return err_response(id, ProtocolError::new("NOT_FOUND", "unknown streamId"))
            }
            Some(e) => {
                e.handle.paused.store(p.paused, std::sync::atomic::Ordering::Relaxed);
                e.state = if p.paused {
                    plugin_core::StreamState::Paused
                } else {
                    plugin_core::StreamState::Playing
                };
                e.state
            }
        }
    };
    state
        .host
        .set_ui_state(p.stream_id, Some(p.paused), None);
    state.broadcast(&Event::stream_state(StreamStateEvent {
        stream_id: p.stream_id,
        state: new_state,
        code: None,
        message: None,
    }));
    ok_response(id, Result_::default())
}

/// Seek executes on the pipeline thread (it owns the demuxer); the reply
/// rides a sync channel with a timeout.
fn seek_stream(state: &Arc<AppState>, id: u64, p: SeekParams) -> String {
    let cmd = state
        .inner
        .lock()
        .unwrap()
        .streams
        .get(&p.stream_id)
        .map(|e| e.handle.cmd_tx.clone());
    let Some(cmd) = cmd else {
        return err_response(id, ProtocolError::new("NOT_FOUND", "unknown streamId"));
    };
    let (rtx, rrx) = std::sync::mpsc::sync_channel(1);
    if cmd
        .send(plugin_core::PipelineCmd::Seek {
            time_ms: p.time_ms,
            reply: rtx,
        })
        .is_err()
    {
        return err_response(id, ProtocolError::new("STREAM_CLOSED", "pipeline gone"));
    }
    match rrx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => ok_response(id, Result_::default()),
        Ok(Err(msg)) => {
            err_response(id, ProtocolError::new("UNSUPPORTED_SEEK", msg))
        }
        Err(_) => err_response(
            id,
            ProtocolError::new("SEEK_TIMEOUT", "pipeline did not answer in 2s"),
        ),
    }
}

fn set_speed(state: &Arc<AppState>, id: u64, p: SpeedParams) -> String {
    let h = state
        .inner
        .lock()
        .unwrap()
        .streams
        .get(&p.stream_id)
        .map(|e| {
            (
                e.handle.speed_permille.clone(),
                e.handle.speed_supported,
            )
        });
    match h {
        None => err_response(id, ProtocolError::new("NOT_FOUND", "unknown streamId")),
        Some((permille, true)) => {
            permille.store((p.speed * 1000.0) as u32, std::sync::atomic::Ordering::Relaxed);
            ok_response(id, Result_::default())
        }
        Some((_, false)) => err_response(
            id,
            ProtocolError::new(
                "UNSUPPORTED_SPEED",
                "speed is server-paced for network streams (GB28181: send platform SIP instead)",
            ),
        ),
    }
}

fn record_stream(state: &Arc<AppState>, id: u64, params: serde_json::Value, start: bool) -> String {
    let p: RecordParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return err_response(id, ProtocolError::new("BAD_PARAMS", e.to_string())),
    };
    let cmd = state
        .inner
        .lock()
        .unwrap()
        .streams
        .get(&p.stream_id)
        .map(|e| e.handle.cmd_tx.clone());
    let Some(cmd) = cmd else {
        return err_response(id, ProtocolError::new("NOT_FOUND", "unknown streamId"));
    };
    let (rtx, rrx) = std::sync::mpsc::sync_channel(1);
    let sent = cmd.send(if start {
        plugin_core::PipelineCmd::RecordStart { reply: rtx }
    } else {
        plugin_core::PipelineCmd::RecordStop { reply: rtx }
    });
    if sent.is_err() {
        return err_response(id, ProtocolError::new("STREAM_CLOSED", "pipeline gone"));
    }
    match rrx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(path)) => {
            state.host.set_ui_state(p.stream_id, None, Some(start));
            ok_response(
                id,
                Result_ {
                    record: Some(RecordResult {
                        path: path.to_string_lossy().into_owned(),
                    }),
                    ..Default::default()
                },
            )
        }
        Ok(Err(msg)) => {
            err_response(id, ProtocolError::new("RECORD_FAILED", msg))
        }
        Err(_) => err_response(
            id,
            ProtocolError::new("RECORD_TIMEOUT", "pipeline did not answer in 5s"),
        ),
    }
}

fn snapshot_stream(state: &Arc<AppState>, id: u64, stream_id: u64) -> String {
    let Some(frame) = state.host.snapshot(stream_id) else {
        return err_response(id, ProtocolError::new("NOT_FOUND", "no frame available"));
    };
    // BGRA → RGBA
    let mut rgba = frame.data.as_ref().clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let img: image::RgbaImage = match image::RgbaImage::from_raw(frame.width, frame.height, rgba) {
        Some(i) => i,
        None => return err_response(id, ProtocolError::new("SNAPSHOT_FAILED", "bad frame")),
    };
    let dir = crate::config::default_data_dir().join("snapshots");
    let _ = std::fs::create_dir_all(&dir);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = dir.join(format!("stream-{stream_id}-{ts}.png"));
    match img.save(&path) {
        Ok(()) => ok_response(
            id,
            Result_ {
                snapshot: Some(SnapshotResult {
                    path: path.to_string_lossy().into_owned(),
                }),
                ..Default::default()
            },
        ),
        Err(e) => err_response(id, ProtocolError::new("SNAPSHOT_FAILED", e.to_string())),
    }
}
