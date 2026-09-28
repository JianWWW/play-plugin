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
/// Consecutive hard decoder errors on the hardware path before auto-fallback
/// to software decode (~2s of video at 25fps).
const HW_ERR_FALLBACK: u32 = 50;
/// Video packets consumed with zero frames out before auto-fallback — the
/// API-visible face of corrupted (花屏) or stalled hardware decode (~15s at
/// 25fps).
const HW_STARVE_FALLBACK: u32 = 375;
/// A connection that lived this long is considered healthy → reset backoff.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);
/// No packet for this long (TCP may still be alive via keep-alive) → treat as
/// a stalled stream and reconnect. Cameras/platforms go silent all the time.
const STALL_TIMEOUT: Duration = Duration::from_secs(15);

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

    // --- video decoder: hardware first, auto-fallback to software ---------
    // Stream parameters are kept around: a mid-stream hw→sw switch rebuilds
    // the software decoder from them without dropping the connection.
    let v_params = video_stream.parameters();
    let mut vpath = if let Some(dev) = device {
        match HwVideoDecoder::attach(video_stream.parameters(), dev) {
            Ok(d) => {
                info!(codec = %video_codec, "hardware decode enabled (d3d11va)");
                VPath::Hw(d)
            }
            Err(e) => {
                warn!(codec = %video_codec, error = %e, "d3d11va unavailable, using software decode");
                VPath::Sw(open_sw(&v_params)?)
            }
        }
    } else {
        VPath::Sw(open_sw(&v_params)?)
    };

    let (v_w, v_h) = match &vpath {
        VPath::Hw(h) => (h.width, h.height),
        VPath::Sw(d) => (d.width(), d.height()),
    };
    let _ = tx.send(PipelineEvent::Info {
        codec: video_codec.clone(),
        width: v_w,
        height: v_h,
        decoder: match &vpath {
            VPath::Hw(_) => "d3d11va",
            VPath::Sw(_) => "sw",
        }
        .into(),
    });
    let _ = tx.send(PipelineEvent::State {
        state: StreamState::Playing,
        code: None,
        message: None,
    });

    // CPU conversion for the software path (any input → BGRA); created
    // lazily from the first decoded frame — the decoder-reported format can
    // still be unknown at open time.
    let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
    let mut scaler_warned = false;
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
    let mut hw_errs: u32 = 0; // consecutive hard errors on the hw path
    let mut hw_starve: u32 = 0; // video packets in with no frame out
    let mut sw_errs: u32 = 0;
    let mut fell_back = false;
    let mut bytes_window: u64 = 0;
    let mut stat_t = Instant::now();
    let mut last_packet = Instant::now();

    let result = (|| -> anyhow::Result<()> {
        for (stream, packet) in ictx.packets() {
            if close.load(Ordering::Relaxed) {
                return Ok(());
            }
            last_packet = Instant::now();
            bytes_window += packet.size() as u64;
            let idx = stream.index();
            if idx == v_index {
                let mut fallback = false;
                match &mut vpath {
                    VPath::Hw(h) => {
                        if h.send(&packet).is_err() {
                            hw_errs += 1;
                        }
                        hw_starve += 1;
                        loop {
                            match h.receive(&mut v_frame) {
                                Ok(true) => {
                                    decoded += 1;
                                    hw_errs = 0;
                                    hw_starve = 0;
                                    if let Some(f) = h.gpu_frame(&v_frame) {
                                        let _ = tx.send(PipelineEvent::Video(f));
                                    }
                                }
                                Ok(false) => break,
                                Err(_) => {
                                    hw_errs += 1;
                                    break;
                                }
                            }
                        }
                        // Auto-switch to software when the hw path keeps
                        // erroring, or keeps eating packets without producing
                        // frames. Mid-stream: demux continues, no reconnect.
                        if !fell_back
                            && (hw_errs >= HW_ERR_FALLBACK || hw_starve >= HW_STARVE_FALLBACK)
                        {
                            fallback = true;
                        }
                    }
                    VPath::Sw(d) => {
                        if d.send_packet(&packet).is_err() {
                            sw_errs += 1;
                        }
                        loop {
                            match d.receive_frame(&mut v_frame) {
                                Ok(()) => {
                                    decoded += 1;
                                    sw_errs = 0;
                                    emit_cpu(
                                        &mut scaler,
                                        &mut scaler_warned,
                                        &v_frame,
                                        &mut bgra,
                                        tx,
                                    );
                                }
                                Err(e) if is_transient(&e) => break,
                                Err(_) => {
                                    sw_errs += 1;
                                    break;
                                }
                            }
                        }
                        // Software is the last rung — if it fails too, error
                        // out so the reconnect loop starts over (fresh hw
                        // attempt included).
                        if sw_errs >= HW_ERR_FALLBACK {
                            return Err(anyhow::anyhow!("software decode failing"));
                        }
                    }
                }
                if fallback {
                    warn!(
                        errors = hw_errs,
                        starved = hw_starve,
                        "hardware decode failing, switching to software mid-stream"
                    );
                    let d = open_sw(&v_params)?;
                    let _ = tx.send(PipelineEvent::Info {
                        codec: video_codec.clone(),
                        width: d.width(),
                        height: d.height(),
                        decoder: "sw(auto-fallback)".into(),
                    });
                    vpath = VPath::Sw(d);
                    fell_back = true;
                    hw_errs = 0;
                    hw_starve = 0;
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
            // Stall watchdog: av_read_frame blocks forever on a silent but
            // keep-alived connection; force a reconnect instead.
            if last_packet.elapsed() >= STALL_TIMEOUT {
                return Err(anyhow::anyhow!(
                    "stream stalled (no data for {}s)",
                    STALL_TIMEOUT.as_secs()
                ));
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

/// Active video decode path. Starts on hardware (d3d11va) when available and
/// can degrade to software mid-stream without dropping the connection.
#[cfg(feature = "ffmpeg")]
enum VPath {
    Hw(HwVideoDecoder),
    Sw(ffmpeg::codec::decoder::video::Video),
}

#[cfg(feature = "ffmpeg")]
fn open_sw(
    params: &ffmpeg::codec::Parameters,
) -> anyhow::Result<ffmpeg::codec::decoder::video::Video> {
    Ok(
        ffmpeg::codec::context::Context::from_parameters(params.clone())?
            .decoder()
            .video()?,
    )
}

/// "Call again later" errors (AVERROR(EAGAIN) surfaces as Other{errno}; EOF
/// is the normal end of a flush) — not decode failures.
#[cfg(feature = "ffmpeg")]
fn is_transient(e: &ffmpeg::Error) -> bool {
    matches!(e, ffmpeg::Error::Other { .. } | ffmpeg::Error::Eof)
}

/// Software path: convert a decoded frame to BGRA and ship it. The scaler is
/// (re)created from the frame itself, so formats unknown at open time and
/// mid-stream resolution changes just work.
#[cfg(feature = "ffmpeg")]
fn emit_cpu(
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    scaler_warned: &mut bool,
    frame: &ffmpeg::util::frame::video::Video,
    bgra: &mut ffmpeg::util::frame::video::Video,
    tx: &mpsc::UnboundedSender<PipelineEvent>,
) {
    let mut run = |scaler: &mut Option<ffmpeg::software::scaling::Context>| {
        scaler
            .as_mut()
            .map(|sc| sc.run(frame, bgra).is_ok())
            .unwrap_or(false)
    };
    let mut ok = run(scaler);
    if !ok {
        // First frame, or format/dimensions changed (InputChanged):
        // (re)build the scaler from the frame itself.
        *scaler = ffmpeg::software::scaling::Context::get(
            frame.format(),
            frame.width(),
            frame.height(),
            ffmpeg::format::Pixel::BGRA,
            frame.width(),
            frame.height(),
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        )
        .ok();
        ok = run(scaler);
    }
    if ok {
        let _ = tx.send(PipelineEvent::Video(VideoFrame::Cpu(CpuFrame {
            width: bgra.width(),
            height: bgra.height(),
            stride: bgra.stride(0),
            data: Arc::new(bgra.data(0).to_vec()),
        })));
    } else if !*scaler_warned {
        *scaler_warned = true;
        warn!("BGRA conversion unavailable; dropping frames");
    }
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
        use windows::core::Interface;
        use windows::Win32::Graphics::Direct3D11::ID3D11Device;

        let mut context = ffmpeg::codec::context::Context::from_parameters(params)?;
        unsafe {
            // Decode on the *render* device: decoded textures then live on
            // the same D3D11 device as the renderer, whose texture handoff
            // (CopySubresourceRegion on its own context) is a valid
            // same-device GPU copy. Letting ffmpeg create its own device
            // (av_hwdevice_ctx_create with NULL) yields textures the render
            // context cannot reach → permanently black window.
            let mut hw_ctx = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if hw_ctx.is_null() {
                anyhow::bail!("av_hwdevice_ctx_alloc failed");
            }
            // AVBufferRef.data → AVHWDeviceContext, whose .hwctx is the
            // d3d11-specific struct (writing through ref.data directly would
            // smash the header fields → AV inside av_hwdevice_ctx_init).
            let dev = (*hw_ctx).data as *mut ffi::AVHWDeviceContext;
            let hwdev = (*dev).hwctx as *mut ffi::AVD3D11VADeviceContext;
            // Hand ffmpeg one reference to the render device (its device_free
            // Releases it); into_raw transfers our AddRef.
            let render_dev: ID3D11Device = device.device().clone();
            (*hwdev).device = render_dev.into_raw() as *mut ffi::ID3D11Device;
            let code = ffi::av_hwdevice_ctx_init(hw_ctx);
            if code != 0 {
                ffi::av_buffer_unref(&mut hw_ctx);
                anyhow::bail!("av_hwdevice_ctx_init failed ({code})");
            }
            let cc = context.as_mut_ptr();
            (*cc).hw_device_ctx = ffi::av_buffer_ref(hw_ctx);
            ffi::av_buffer_unref(&mut hw_ctx);
            (*cc).get_format = Some(hw_get_format);
        }
        // `Context::from_parameters` leaves ctx->codec NULL, and
        // `Decoder::open()` then calls avcodec_open2 with no codec → EINVAL
        // (why d3d11va silently never engaged before). Find the decoder
        // explicitly and open with it.
        let codec = ffmpeg::codec::decoder::find(context.id())
            .ok_or_else(|| anyhow::anyhow!("no decoder for stream"))?;
        let inner = context.decoder().open_as(codec)?.video()?;
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

    fn send(&mut self, packet: &ffmpeg::codec::packet::Packet) -> Result<(), ffmpeg::Error> {
        self.inner.send_packet(packet)
    }

    /// Ok(true) = frame decoded, Ok(false) = nothing more (EAGAIN),
    /// Err = hard decode error.
    fn receive(
        &mut self,
        frame: &mut ffmpeg::util::frame::video::Video,
    ) -> Result<bool, ffmpeg::Error> {
        match self.inner.receive_frame(frame) {
            Ok(()) => Ok(true),
            Err(e) if is_transient(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Wraps the decoder's D3D11 texture (texture array + subresource) so the
    /// renderer can copy it GPU-side without any CPU roundtrip.
    fn gpu_frame(&mut self, frame: &ffmpeg::util::frame::video::Video) -> Option<VideoFrame> {
        unsafe {
            let av = frame.as_ptr();
            if (*av).format != ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as std::ffi::c_int {
                use std::sync::atomic::{AtomicBool, Ordering};
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    warn!(format = (*av).format, "decoded frame is not D3D11; gpu path skipped");
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduces the hw-decode attach FFI sequence against the real render
    /// device: create device (VIDEO_SUPPORT), hand it to FFmpeg, init.
    #[test]
    fn hw_device_init_on_render_device() {
        let gpu = gpu::GpuContext::create().expect("render device");
        unsafe {
            let mut hw_ctx =
                ffmpeg::ffi::av_hwdevice_ctx_alloc(ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            assert!(!hw_ctx.is_null());
            let dev = (*hw_ctx).data as *mut ffmpeg::ffi::AVHWDeviceContext;
            let hwdev = (*dev).hwctx as *mut ffmpeg::ffi::AVD3D11VADeviceContext;
            use windows::core::Interface;
            use windows::Win32::Graphics::Direct3D11::ID3D11Device;
            let render_dev: ID3D11Device = gpu.device().clone();
            (*hwdev).device = render_dev.into_raw() as *mut ffmpeg::ffi::ID3D11Device;
            eprintln!("[test] device set, init…");
            let code = ffmpeg::ffi::av_hwdevice_ctx_init(hw_ctx);
            eprintln!("[test] init code={code}");
            ffmpeg::ffi::av_buffer_unref(&mut hw_ctx);
            assert_eq!(code, 0, "av_hwdevice_ctx_init failed");
        }
    }
}
