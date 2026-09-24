//! FFmpeg-backed pipeline: RTSP (over TCP) / HTTP-FLV / HLS demux + decode.
//!
//! Decode strategy: D3D11VA hardware decode attached to the shared render
//! device — decoded frames stay in GPU textures (zero CPU copy). If hardware
//! init fails for a stream, it transparently falls back to software decode
//! with a BGRA conversion on CPU.
//!
//! Latency: `nobuffer`/`low_delay` flags, TCP transport, frames are handed to
//! the renderer as soon as they arrive (latest-wins, no reorder queue).

use crate::{
    gpu, AudioChunk, CpuFrame, PipelineEvent, PipelineHandle, StreamOptions, StreamState,
    VideoFrame,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

#[cfg(feature = "ffmpeg")]
use ffmpeg_next as ffmpeg;

/// Reconnect backoff (seconds), with jitter, capped at the last entry.
const BACKOFF_SECS: [f32; 5] = [0.5, 1.0, 2.0, 4.0, 8.0];
/// A connection that lived this long is considered healthy → reset backoff.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);

pub fn spawn(
    opts: StreamOptions,
    device: Option<gpu::GpuContext>,
    tx: mpsc::UnboundedSender<PipelineEvent>,
) -> PipelineHandle {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = ffmpeg::init();
    });

    let close = Arc::new(AtomicBool::new(false));
    let muted = Arc::new(AtomicBool::new(opts.muted));
    let close2 = close.clone();
    let muted2 = muted.clone();
    let raw_url = opts.url.clone();
    let label = crate::redact_url(&raw_url)
        .chars()
        .take(32)
        .collect::<String>();
    let _ = std::thread::Builder::new()
        .name(format!("pl-{label}"))
        .spawn(move || {
            run_loop(close2, muted2, &raw_url, device, tx);
        });
    PipelineHandle { close, muted }
}

fn run_loop(
    close: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    url: &str,
    device: Option<gpu::GpuContext>,
    tx: mpsc::UnboundedSender<PipelineEvent>,
) {
    let mut consecutive_failures: usize = 0;
    while !close.load(Ordering::Relaxed) {
        let started = Instant::now();
        match run_once(&close, &muted, url, device.clone(), &tx) {
            Ok(()) => {
                let _ = tx.send(PipelineEvent::State {
                    state: StreamState::Stopped,
                    code: None,
                    message: None,
                });
                return; // closed by caller
            }
            Err(e) => {
                if close.load(Ordering::Relaxed) {
                    return;
                }
                warn!(url = %url, error = %e, "stream error, reconnecting");
                let _ = tx.send(PipelineEvent::State {
                    state: StreamState::Reconnecting,
                    code: Some("STREAM_ERROR"),
                    message: Some(e.to_string()),
                });
                if started.elapsed() >= BACKOFF_RESET_AFTER {
                    consecutive_failures = 0;
                }
                let base = BACKOFF_SECS[consecutive_failures.min(BACKOFF_SECS.len() - 1)];
                let jitter = rand::random::<f32>() * 0.3 * base;
                let deadline = Instant::now() + Duration::from_secs_f32(base + jitter);
                while Instant::now() < deadline && !close.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                consecutive_failures += 1;
            }
        }
    }
}

fn run_once(
    close: &AtomicBool,
    muted: &AtomicBool,
    url: &str,
    device: Option<gpu::GpuContext>,
    tx: &mpsc::UnboundedSender<PipelineEvent>,
) -> anyhow::Result<()> {
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("rtsp_transport", "tcp");
    opts.set("fflags", "nobuffer");
    opts.set("flags", "low_delay");
    opts.set("max_delay", "0");
    opts.set("timeout", "10000000"); // socket IO timeout (µs), RTSP/HTTP
    opts.set("reconnect", "1");
    opts.set("reconnect_streamed", "1");

    let _ = tx.send(PipelineEvent::State {
        state: StreamState::Connecting,
        code: None,
        message: None,
    });
    let mut ictx = ffmpeg::format::input_with_dictionary(url, opts)?;

    let video_stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let v_index = video_stream.index();
    let video_codec = ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?
        .id()
        .name()
        .to_string();

    let audio_stream = ictx.streams().best(ffmpeg::media::Type::Audio);
    let a_index = audio_stream
        .as_ref()
        .map(|s| s.index())
        .unwrap_or(usize::MAX);
    let audio_params = audio_stream.map(|s| s.parameters());

    // --- video decoder: hardware first, software fallback -----------------
    let mut hw: Option<HwVideoDecoder> = None;
    if let Some(dev) = device {
        match HwVideoDecoder::attach(video_stream.parameters(), dev) {
            Ok(d) => {
                info!(codec = %video_codec, "hardware decode enabled (d3d11va)");
                hw = Some(d);
            }
            Err(e) => {
                warn!(codec = %video_codec, error = %e, "d3d11va unavailable, using software decode")
            }
        }
    }
    let mut sw = if hw.is_none() {
        Some(
            ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?
                .decoder()
                .video()?,
        )
    } else {
        None
    };

    let (v_w, v_h) = if let Some(h) = &hw {
        (h.width, h.height)
    } else {
        let d = sw.as_ref().unwrap();
        (d.width(), d.height())
    };
    let decoder_name = if hw.is_some() { "d3d11va" } else { "sw" };
    let _ = tx.send(PipelineEvent::Info {
        codec: video_codec.clone(),
        width: v_w,
        height: v_h,
        decoder: decoder_name.into(),
    });
    let _ = tx.send(PipelineEvent::State {
        state: StreamState::Playing,
        code: None,
        message: None,
    });

    // CPU conversion context for the software path (any input → BGRA).
    let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
    if let Some(d) = sw.as_ref() {
        match ffmpeg::software::scaling::Context::get(
            d.format(),
            d.width(),
            d.height(),
            ffmpeg::format::Pixel::BGRA,
            d.width(),
            d.height(),
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        ) {
            Ok(c) => scaler = Some(c),
            Err(e) => warn!(error = %e, "scaler init failed"),
        }
    }
    let mut bgra = ffmpeg::util::frame::video::Video::empty();

    // --- audio decoder → f32 / 48kHz / stereo (optional) -------------------
    let mut a_decoder = audio_params
        .and_then(|p| ffmpeg::codec::context::Context::from_parameters(p).ok())
        .and_then(|c| c.decoder().audio().ok());
    let mut resampler = a_decoder.as_ref().and_then(|ad| {
        ffmpeg::software::resampling::Context::get(
            ad.format(),
            ad.channel_layout(),
            ad.rate(),
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            ffmpeg::ChannelLayout::STEREO,
            48_000,
        )
        .ok()
    });
    let mut a_frame = ffmpeg::util::frame::audio::Audio::empty();
    let mut a_out = ffmpeg::util::frame::audio::Audio::empty();

    let mut v_frame = ffmpeg::util::frame::video::Video::empty();
    let mut decoded: u64 = 0;
    let mut bytes_window: u64 = 0;
    let mut stat_t = Instant::now();

    let result = (|| -> anyhow::Result<()> {
        for (stream, packet) in ictx.packets() {
            if close.load(Ordering::Relaxed) {
                return Ok(());
            }
            bytes_window += packet.size() as u64;
            let idx = stream.index();
            if idx == v_index {
                if let Some(h) = hw.as_mut() {
                    if h.send(&packet) {
                        while h.receive(&mut v_frame) {
                            decoded += 1;
                            if let Some(f) = h.gpu_frame(&v_frame) {
                                let _ = tx.send(PipelineEvent::Video(f));
                            }
                        }
                    }
                } else if let Some(d) = sw.as_mut() {
                    if d.send_packet(&packet).is_ok() {
                        while d.receive_frame(&mut v_frame).is_ok() {
                            decoded += 1;
                            if let Some(sc) = scaler.as_mut() {
                                if sc.run(&v_frame, &mut bgra).is_ok() {
                                    let _ =
                                        tx.send(PipelineEvent::Video(VideoFrame::Cpu(CpuFrame {
                                            width: bgra.width(),
                                            height: bgra.height(),
                                            stride: bgra.stride(0),
                                            data: Arc::new(bgra.data(0).to_vec()),
                                        })));
                                }
                            }
                        }
                    }
                }
            } else if idx == a_index {
                if let (Some(ad), Some(rs)) = (a_decoder.as_mut(), resampler.as_mut()) {
                    if ad.send_packet(&packet).is_ok() {
                        while ad.receive_frame(&mut a_frame).is_ok() {
                            if rs.run(&a_frame, &mut a_out).is_ok()
                                && !muted.load(Ordering::Relaxed)
                            {
                                let samples = f32_from_le(a_out.data(0));
                                if !samples.is_empty() {
                                    let _ = tx.send(PipelineEvent::Audio(AudioChunk {
                                        samples: Arc::new(samples),
                                        sample_rate: 48_000,
                                        channels: 2,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            if stat_t.elapsed() >= Duration::from_secs(1) {
                let elapsed = stat_t.elapsed().as_secs_f32();
                let _ = tx.send(PipelineEvent::Stats {
                    fps: decoded as f32 / elapsed,
                    bitrate_kbps: bytes_window as f64 * 8.0 / 1000.0 / elapsed as f64,
                });
                decoded = 0;
                bytes_window = 0;
                stat_t = Instant::now();
            }
        }
        Err(anyhow::anyhow!("stream ended (EOF)"))
    })();

    // Re-send the interior error so the reconnect loop sees it.
    if close.load(Ordering::Relaxed) {
        Ok(())
    } else {
        result
    }
}

fn f32_from_le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ---------------------------------------------------------------------------

/// D3D11VA hardware video decoder attached to the shared render device.
#[cfg(feature = "ffmpeg")]
struct HwVideoDecoder {
    inner: ffmpeg::codec::decoder::video::Video,
    device: gpu::GpuContext,
    width: u32,
    height: u32,
    last_subresource: u32,
}

#[cfg(feature = "ffmpeg")]
impl HwVideoDecoder {
    /// Configures the raw AVCodecContext for D3D11VA *before* opening.
    fn attach(params: ffmpeg::codec::Parameters, device: gpu::GpuContext) -> anyhow::Result<Self> {
        use ffmpeg::ffi;
        use std::ptr;

        let mut context = ffmpeg::codec::context::Context::from_parameters(params)?;
        unsafe {
            let mut hw_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
            let code = ffi::av_hwdevice_ctx_create(
                &mut hw_ctx,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if code != 0 || hw_ctx.is_null() {
                anyhow::bail!("av_hwdevice_ctx_create failed ({code})");
            }
            let cc = context.as_mut_ptr();
            (*cc).hw_device_ctx = ffi::av_buffer_ref(hw_ctx);
            (*cc).get_format = Some(hw_get_format);
        }
        let inner = context.decoder().open()?.video()?;
        let (width, height) = (inner.width(), inner.height());
        if width == 0 || height == 0 {
            anyhow::bail!("invalid video dimensions");
        }
        Ok(Self {
            inner,
            device,
            width,
            height,
            last_subresource: 0,
        })
    }

    fn send(&mut self, packet: &ffmpeg::codec::packet::Packet) -> bool {
        self.inner.send_packet(packet).is_ok()
    }

    fn receive(&mut self, frame: &mut ffmpeg::util::frame::video::Video) -> bool {
        self.inner.receive_frame(frame).is_ok()
    }

    /// Wraps the decoder's D3D11 texture (texture array + subresource) so the
    /// renderer can copy it GPU-side without any CPU roundtrip.
    fn gpu_frame(&mut self, frame: &ffmpeg::util::frame::video::Video) -> Option<VideoFrame> {
        unsafe {
            let av = frame.as_ptr();
            if (*av).format != ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as std::ffi::c_int {
                return None;
            }
            let raw = (*av).data[0] as *mut windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
            if raw.is_null() {
                return None;
            }
            self.last_subresource = (*av).data[1] as usize as u32;
            // Borrow the raw COM pointer and clone it (AddRef) into an owned handle.
            use windows::core::Interface;
            let raw_void = raw as *mut std::ffi::c_void;
            let borrowed =
                windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::from_raw_borrowed(
                    &raw_void,
                )?;
            let owned = borrowed.clone();
            Some(VideoFrame::Gpu(
                self.device.wrap_texture(owned),
                self.last_subresource,
                self.width,
                self.height,
            ))
        }
    }
}

#[cfg(feature = "ffmpeg")]
unsafe extern "C" fn hw_get_format(
    _ctx: *mut ffmpeg::ffi::AVCodecContext,
    fmts: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    use ffmpeg::ffi;
    let mut p = fmts;
    while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *p == ffi::AVPixelFormat::AV_PIX_FMT_D3D11 {
            return *p;
        }
        p = p.add(1);
    }
    *fmts
}
