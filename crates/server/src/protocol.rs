//! Wire protocol v1. JSON text frames only (no video data traverses the
//! WebSocket — frames stay native). Shapes mirror the TS SDK in `/sdk`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenParams {
    #[serde(rename = "streamId")]
    pub stream_id: u64,
    pub url: String,
    pub rect: Rect,
    #[serde(default = "default_true")]
    pub muted: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct RectParams {
    #[serde(rename = "streamId")]
    pub stream_id: u64,
    pub rect: Rect,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MuteParams {
    #[serde(rename = "streamId")]
    pub stream_id: u64,
    pub muted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotParams {
    #[serde(rename = "streamId")]
    pub stream_id: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HelloResult {
    pub plugin: String,
    pub version: String,
    pub protocol: u32,
    #[serde(rename = "maxStreams")]
    pub max_streams: u32,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub h264: bool,
    pub h265: bool,
    pub rtsp: bool,
    pub flv: bool,
    pub hls: bool,
    pub hardware_decode: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotResult {
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResult {
    pub version: String,
    #[serde(rename = "activeStreams")]
    pub active_streams: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamStateEvent {
    #[serde(rename = "streamId")]
    pub stream_id: u64,
    pub state: plugin_core::StreamState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamInfoEvent {
    #[serde(rename = "streamId")]
    pub stream_id: u64,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub decoder: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamStatsEvent {
    #[serde(rename = "streamId")]
    pub stream_id: u64,
    pub fps: f32,
    #[serde(rename = "bitrateKbps")]
    pub bitrate_kbps: f64,
    pub dropped: u64,
    pub decoder: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateEvent {
    pub version: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub v: u32,
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Result_::is_none")]
    pub result: Result_,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Result_ {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hello: Option<HelloResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pong: Option<u64>,
}

impl Result_ {
    pub fn is_none(&self) -> bool {
        self.hello.is_none()
            && self.snapshot.is_none()
            && self.status.is_none()
            && self.pong.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolError {
    pub code: &'static str,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub v: u32,
    pub event: String,
    pub params: serde_json::Value,
}

impl Event {
    pub fn new(event: &str, params: serde_json::Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            event: event.into(),
            params,
        }
    }

    pub fn stream_state(p: StreamStateEvent) -> Self {
        Self::new("stream.state", serde_json::to_value(p).unwrap_or_default())
    }

    pub fn stream_info(p: StreamInfoEvent) -> Self {
        Self::new("stream.info", serde_json::to_value(p).unwrap_or_default())
    }

    pub fn stream_stats(p: StreamStatsEvent) -> Self {
        Self::new("stream.stats", serde_json::to_value(p).unwrap_or_default())
    }

    pub fn update_available(p: UpdateEvent) -> Self {
        Self::new(
            "app.updateAvailable",
            serde_json::to_value(p).unwrap_or_default(),
        )
    }
}

pub fn ok_response(id: u64, result: Result_) -> String {
    serde_json::to_string(&Response {
        v: PROTOCOL_VERSION,
        id,
        ok: true,
        result,
        error: None,
    })
    .unwrap_or_default()
}

pub fn err_response(id: u64, error: ProtocolError) -> String {
    serde_json::to_string(&Response {
        v: PROTOCOL_VERSION,
        id,
        ok: false,
        result: Result_::default(),
        error: Some(error),
    })
    .unwrap_or_default()
}

/// URL schemes accepted by `stream.open`.
pub fn is_supported_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("rtsp://")
        || lower.starts_with("rtsps://")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("test://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let raw = r#"{"v":1,"id":7,"method":"stream.open","params":{"streamId":42,"url":"rtsp://x/y","rect":{"l":0,"t":0,"w":100,"h":50},"muted":false}}"#;
        let req: Request = serde_json::from_str(raw).unwrap();
        assert_eq!(req.method, "stream.open");
        assert_eq!(req.id, 7);
        let open: OpenParams = serde_json::from_value(req.params).unwrap();
        assert_eq!(open.stream_id, 42);
        assert!(!open.muted);
        assert_eq!(open.rect.w, 100);
    }

    #[test]
    fn response_shape() {
        let s = err_response(3, ProtocolError::new("LIMIT_STREAMS", "too many"));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "LIMIT_STREAMS");
        assert_eq!(v["v"], 1);
    }

    #[test]
    fn event_shape() {
        let e = Event::stream_state(StreamStateEvent {
            stream_id: 1,
            state: plugin_core::StreamState::Playing,
            code: None,
            message: None,
        });
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["event"], "stream.state");
        assert_eq!(v["params"]["streamId"], 1);
        assert_eq!(v["params"]["state"], "playing");
    }

    #[test]
    fn url_schemes() {
        assert!(is_supported_url("rtsp://cam/stream"));
        assert!(is_supported_url("RTSP://cam/stream"));
        assert!(is_supported_url("http://host/live.flv"));
        assert!(is_supported_url("test://pattern"));
        assert!(!is_supported_url("file:///c:/x.mp4"));
        assert!(!is_supported_url("javascript:alert(1)"));
    }

    #[test]
    fn rect_defaults_and_names() {
        let raw = r#"{"streamId":1,"rect":{"l":-5,"t":10,"w":1920,"h":1080}}"#;
        let p: RectParams = serde_json::from_str(raw).unwrap();
        assert!(!p.hidden);
        assert_eq!(p.rect.l, -5);
    }
}
