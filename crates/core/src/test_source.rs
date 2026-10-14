//! Synthetic animated source for dev/CI: `test://pattern?w=1280&h=720&fps=30`.
//! No network, no FFmpeg — used by smoke tests and integration tests.

use crate::{CpuFrame, PipelineCmd, PipelineEvent, PipelineHandle, StreamOptions, StreamState, VideoFrame};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub fn spawn(opts: &StreamOptions, tx: mpsc::UnboundedSender<PipelineEvent>) -> PipelineHandle {
    let close = Arc::new(AtomicBool::new(false));
    let muted = Arc::new(AtomicBool::new(opts.muted));
    let paused = Arc::new(AtomicBool::new(false));
    let speed_permille = Arc::new(AtomicU32::new(1000));
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<PipelineCmd>();
    let (w, h, fps) = parse_query(&opts.url);
    let close2 = close.clone();
    let paused2 = paused.clone();
    let speed2 = speed_permille.clone();
    let _ = std::thread::Builder::new()
        .name(format!("test-src-{w}x{h}"))
        .spawn(move || {
            run(
                close2,
                paused2,
                speed2,
                cmd_rx,
                w,
                h,
                fps,
                tx,
            )
        });
    PipelineHandle {
        close,
        muted,
        paused,
        speed_permille,
        cmd_tx,
        speed_supported: true,
    }
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

fn run(
    close: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    speed_permille: Arc<AtomicU32>,
    cmd_rx: std::sync::mpsc::Receiver<PipelineCmd>,
    w: u32,
    h: u32,
    fps: f32,
    tx: mpsc::UnboundedSender<PipelineEvent>,
) {
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

    let base_dur = Duration::from_secs_f32(1.0 / fps);
    let stride = (w as usize) * 4;
    let mut i: u64 = 0;
    let mut last_stats = Instant::now();
    while !close.load(Ordering::Relaxed) {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                PipelineCmd::Seek { time_ms, reply } => {
                    // Pattern phase restarts at the seek point.
                    i = (time_ms.max(0) as f64 * fps as f64 / 1000.0) as u64;
                    let _ = reply.send(Ok(()));
                }
                PipelineCmd::RecordStart { reply } => {
                    let _ = reply.send(Err("recording not supported on test source".into()));
                }
                PipelineCmd::RecordStop { reply } => {
                    let _ = reply.send(Err("not recording".into()));
                }
            }
        }
        let speed = speed_permille.load(Ordering::Relaxed).max(1) as f32 / 1000.0;
        let frame_dur = Duration::from_secs_f32(base_dur.as_secs_f32() / speed);
        if paused.load(Ordering::Relaxed) {
            // Display freeze: no frames out, stats report zero.
            if last_stats.elapsed() >= Duration::from_secs(1) {
                last_stats = Instant::now();
                let _ = tx.send(PipelineEvent::Stats {
                    fps: 0.0,
                    bitrate_kbps: 0.0,
                });
            }
            std::thread::sleep(frame_dur);
            continue;
        }
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
                fps: fps * speed,
                bitrate_kbps: (w as f64 * h as f64 * 4.0 * (fps * speed) as f64 * 8.0) / 1000.0,
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

#[cfg(test)]
mod tests {
    use crate::{PipelineCmd, PipelineEvent, StreamOptions};
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn drain(
        rx: &mut mpsc::UnboundedReceiver<PipelineEvent>,
        frames: &mut u64,
        fps: &mut f32,
    ) {
        while let Ok(ev) = rx.try_recv() {
            match ev {
                PipelineEvent::Video(_) => *frames += 1,
                PipelineEvent::Stats { fps: f, .. } => *fps = f,
                _ => {}
            }
        }
    }

    #[test]
    fn pause_freezes_speed_accelerates() {
        let (tx, mut rx) = mpsc::unbounded_channel::<PipelineEvent>();
        let h = super::spawn(
            &StreamOptions {
                url: "test://pattern?w=64&h=64&fps=10".into(),
                muted: true,
                title: String::new(),
            },
            tx,
        );
        let (mut frames, mut fps) = (0u64, 0f32);
        std::thread::sleep(Duration::from_millis(700));
        drain(&mut rx, &mut frames, &mut fps);
        assert!(frames > 3, "playing: {frames} frames in 0.7s @10fps");

        // Pause: display freeze — no new frames, stats report fps 0.
        h.paused.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(1200));
        let mut f2 = frames;
        drain(&mut rx, &mut f2, &mut fps);
        assert_eq!(f2, frames, "paused: no frames delivered");
        assert_eq!(fps, 0.0, "paused: stats fps is 0");

        // Resume + 4x speed: ~40fps.
        h.paused.store(false, Ordering::Relaxed);
        h.speed_permille.store(4000, Ordering::Relaxed);
        let before = f2;
        std::thread::sleep(Duration::from_millis(1200));
        drain(&mut rx, &mut frames, &mut fps);
        let rate = (frames - before) as f32 / 1.2;
        assert!(rate > 25.0, "4x speed: {rate} fps expected ~40");

        // Seek: acknowledged (resets pattern phase).
        let (rtx, rrx) = std::sync::mpsc::sync_channel(1);
        h.cmd_tx
            .send(PipelineCmd::Seek {
                time_ms: 0,
                reply: rtx,
             })
            .unwrap();
        assert!(rrx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());

        // Record: honestly refused on a synthetic source.
        let (rtx, rrx) = std::sync::mpsc::sync_channel(1);
        h.cmd_tx
            .send(PipelineCmd::RecordStart { reply: rtx })
            .unwrap();
        assert!(rrx.recv_timeout(Duration::from_secs(2)).unwrap().is_err());

        h.close.store(true, Ordering::Relaxed);
    }
}
