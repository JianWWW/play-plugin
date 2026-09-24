//! Synthetic animated source for dev/CI: `test://pattern?w=1280&h=720&fps=30`.
//! No network, no FFmpeg — used by smoke tests and integration tests.

use crate::{CpuFrame, PipelineEvent, PipelineHandle, StreamOptions, StreamState, VideoFrame};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub fn spawn(opts: &StreamOptions, tx: mpsc::UnboundedSender<PipelineEvent>) -> PipelineHandle {
    let close = Arc::new(AtomicBool::new(false));
    let muted = Arc::new(AtomicBool::new(opts.muted));
    let (w, h, fps) = parse_query(&opts.url);
    let close2 = close.clone();
    let _ = std::thread::Builder::new()
        .name(format!("test-src-{w}x{h}"))
        .spawn(move || run(close2, w, h, fps, tx));
    PipelineHandle { close, muted }
}

fn parse_query(url: &str) -> (u32, u32, f32) {
    let (mut w, mut h, mut fps) = (640u32, 360u32, 30.0f32);
    if let Some(q) = url.split_once('?').map(|(_, q)| q) {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "w" => w = v.parse().unwrap_or(w),
                    "h" => h = v.parse().unwrap_or(h),
                    "fps" => fps = v.parse().unwrap_or(fps),
                    _ => {}
                }
            }
        }
    }
    (w.clamp(64, 3840), h.clamp(64, 2160), fps.clamp(1.0, 120.0))
}

fn run(close: Arc<AtomicBool>, w: u32, h: u32, fps: f32, tx: mpsc::UnboundedSender<PipelineEvent>) {
    let _ = tx.send(PipelineEvent::State {
        state: StreamState::Connecting,
        code: None,
        message: None,
    });
    std::thread::sleep(Duration::from_millis(120));
    let _ = tx.send(PipelineEvent::Info {
        codec: "test".into(),
        width: w,
        height: h,
        decoder: "test".into(),
    });
    let _ = tx.send(PipelineEvent::State {
        state: StreamState::Playing,
        code: None,
        message: None,
    });

    let frame_dur = Duration::from_secs_f32(1.0 / fps);
    let stride = (w as usize) * 4;
    let mut i: u64 = 0;
    let mut last_stats = Instant::now();
    while !close.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let mut buf = vec![0u8; stride * h as usize];
        draw(&mut buf, stride, w, h, i);
        let _ = tx.send(PipelineEvent::Video(VideoFrame::Cpu(CpuFrame {
            width: w,
            height: h,
            stride,
            data: Arc::new(buf),
        })));
        i += 1;
        if last_stats.elapsed() >= Duration::from_secs(1) {
            last_stats = Instant::now();
            let _ = tx.send(PipelineEvent::Stats {
                fps,
                bitrate_kbps: (w as f64 * h as f64 * 4.0 * fps as f64 * 8.0) / 1000.0,
            });
        }
        let spent = t0.elapsed();
        if spent < frame_dur {
            std::thread::sleep(frame_dur - spent);
        }
    }
    let _ = tx.send(PipelineEvent::State {
        state: StreamState::Stopped,
        code: None,
        message: None,
    });
}

/// Gradient + checkerboard + moving white bar so motion/tearing is obvious.
fn draw(buf: &mut [u8], stride: usize, w: u32, h: u32, frame: u64) {
    for y in 0..h as usize {
        let row = &mut buf[y * stride..y * stride + (w as usize) * 4];
        for x in 0..w as usize {
            let (r, g, b) = if (x / 32 + y / 32 + frame as usize / 10).is_multiple_of(2) {
                (30u8, 90u8, 160u8)
            } else {
                (20u8, 60u8, 110u8)
            };
            let (mut r, mut g, mut b) = (
                r + (x * 40 / w as usize) as u8,
                g + (y * 40 / h as usize) as u8,
                b,
            );
            let bar_x = ((frame as f64 / 2.0) % w as f64) as usize;
            if x >= bar_x && x < bar_x + 8 {
                (r, g, b) = (255, 255, 255);
            }
            let px = x * 4;
            // BGRA byte order.
            row[px] = b;
            row[px + 1] = g;
            row[px + 2] = r;
            row[px + 3] = 255;
        }
    }
}
