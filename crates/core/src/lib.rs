//! Stream pipeline abstraction: one pipeline per live stream, independent
//! thread, latest-wins frame delivery. Backends: FFmpeg (RTSP/HTTP-FLV/HLS)
//! and a synthetic test source (`test://pattern?w=&h=&fps=`) for dev/CI.

#[cfg(feature = "ffmpeg")]
pub mod ffmpeg_backend;
pub mod gpu;
pub mod test_source;

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamState {
    Connecting,
    Playing,
    Reconnecting,
    Stopped,
    Error,
}

/// CPU frame in BGRA byte order (matches DXGI_FORMAT_B8G8R8A8_UNORM layout).
#[derive(Clone)]
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: usize,
    pub data: Arc<Vec<u8>>,
}

/// A video frame: decoded either to CPU memory or living as a D3D11 texture
/// (hardware decode path, zero CPU copy) created on the shared render device.
#[derive(Clone)]
pub enum VideoFrame {
    Cpu(CpuFrame),
    /// (texture, subresource index, width, height)
    Gpu(gpu::GpuTexture, u32, u32, u32),
}

#[derive(Clone)]
pub struct AudioChunk {
    pub samples: Arc<Vec<f32>>,
    pub sample_rate: u32,
    pub channels: u16,
}

pub enum PipelineEvent {
    Info {
        codec: String,
        width: u32,
        height: u32,
        decoder: String,
    },
    Stats {
        fps: f32,
        bitrate_kbps: f64,
    },
    State {
        state: StreamState,
        code: Option<&'static str>,
        message: Option<String>,
    },
    Video(VideoFrame),
    Audio(AudioChunk),
}

pub struct StreamOptions {
    pub url: String,
    pub muted: bool,
}

pub struct PipelineHandle {
    pub close: Arc<AtomicBool>,
    pub muted: Arc<AtomicBool>,
}

/// Spawns one pipeline thread. `device` enables the zero-copy hardware decode
/// path (decoder attaches to the render device).
pub fn spawn_stream(
    opts: StreamOptions,
    device: Option<gpu::GpuContext>,
    force_test_source: bool,
    tx: mpsc::UnboundedSender<PipelineEvent>,
) -> PipelineHandle {
    if force_test_source || opts.url.starts_with("test://") {
        test_source::spawn(&opts, tx)
    } else {
        #[cfg(feature = "ffmpeg")]
        return ffmpeg_backend::spawn(opts, device, tx);
        #[cfg(not(feature = "ffmpeg"))]
        {
            let _ = device;
            let _ = tx.send(PipelineEvent::State {
                state: StreamState::Error,
                code: Some("UNSUPPORTED"),
                message: Some("plugin built without ffmpeg backend".into()),
            });
            PipelineHandle {
                close: Arc::new(AtomicBool::new(false)),
                muted: Arc::new(AtomicBool::new(opts.muted)),
            }
        }
    }
}

/// Useful for stats: shared counter of frames dropped because the renderer was
/// still busy with the previous one (latest-wins policy).
pub type DropCounter = Arc<AtomicU64>;

pub fn new_drop_counter() -> DropCounter {
    Arc::new(AtomicU64::new(0))
}

/// Strips `user:pass@` credentials from a URL so it can safely appear in logs.
pub fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        if rest.find('@').is_some() {
            // Only redact if an '@' appears before the first '/'.
            let host_part = rest.split('/').next().unwrap_or("");
            if let Some(at_in_host) = host_part.rfind('@') {
                return format!("{}***@{}", &url[..scheme_end + 3], &rest[at_in_host + 1..]);
            }
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn redacts_credentials() {
        assert_eq!(
            redact_url("rtsp://admin:secret123@cam.local:554/Streaming/Channels/101"),
            "rtsp://***@cam.local:554/Streaming/Channels/101"
        );
        assert_eq!(
            redact_url("rtsp://cam.local/stream"),
            "rtsp://cam.local/stream"
        );
        assert_eq!(redact_url("http://a@b.com/x"), "http://***@b.com/x");
    }
}
