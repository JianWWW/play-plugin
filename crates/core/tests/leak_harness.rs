//! Leak isolation harness (ignored): spawn ONLY the ffmpeg pipeline against a
//! local stream and print process memory slope. No overlay, no server.
//!
//!   cargo test -p plugin-core --test leak_harness --release -- --ignored --nocapture

use plugin_core::{spawn_stream, PipelineEvent, StreamOptions};
use std::time::{Duration, Instant};

fn mem_mb() -> f64 {
    // PROCESS_MEMORY_COUNTERS via tasklist-free route: use /proc? Windows: use
    // powershell-free GetProcessMemoryInfo through std — not available; read
    // from the OS via `tasklist` output instead.
    let out = std::process::Command::new("tasklist")
        .args([
            "/FI",
            &format!("PID eq {}", std::process::id()),
            "/FO",
            "CSV",
            "/NH",
        ])
        .output()
        .expect("tasklist");
    let line = String::from_utf8_lossy(&out.stdout);
    // "leak_harness.exe","PID","Console","1","77,832 K" — memory may contain a comma
    let parts: Vec<&str> = line.split(',').collect();
    let mem_str: String = parts
        .iter()
        .skip(4)
        .flat_map(|s| s.chars().filter(|c| c.is_ascii_digit()))
        .collect();
    let mem = mem_str.parse::<u64>().unwrap_or(0);
    mem as f64 / 1024.0
}

fn run(label: &str, url: &str, secs: u64) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let h = spawn_stream(
        StreamOptions { url: url.into(), muted: true, title: String::new() },
        None,
        false,
        tx,
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut frames: u64 = 0;
    rt.block_on(async {
        let t0 = Instant::now();
        let mut last_report = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            tokio::select! {
                ev = rx.recv() => {
                    if let Some(PipelineEvent::Video(_)) = ev { frames += 1; }
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                last_report = Instant::now();
                println!("[{label}] {:+?} mem={:.0}MB frames={frames}", t0.elapsed(), mem_mb());
            }
        }
    });
    h.close.store(true, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(500));
    println!("[{label}] DONE frames={frames}");
}

#[test]
#[ignore = "needs local mediamtx + publisher"]
fn leak_playing() {
    let url = std::env::var("LEAK_URL").unwrap_or("rtsp://127.0.0.1:8554/cam".into());
    run("playing", &url, 40);
}

/// Pure-FFI read loop (no ffmpeg-next wrappers, no decode): isolates the
/// custom-built DLLs. If this leaks, the leak is inside avformat itself.
#[test]
#[ignore = "needs local mediamtx"]
fn leak_ffi_raw() {
    use std::ffi::CString;
    #[cfg(feature = "ffmpeg")]
    unsafe {
        let url = CString::new(std::env::var("RAW_URL").unwrap_or("rtsp://127.0.0.1:8554/cam".into())).unwrap();
        let mut ictx: *mut ffmpeg_next::ffi::AVFormatContext = std::ptr::null_mut();
        let mut dict: *mut ffmpeg_next::ffi::AVDictionary = std::ptr::null_mut();
        ffmpeg_next::ffi::av_dict_set(&mut dict, b"rtsp_transport\0".as_ptr() as *const _, b"tcp\0".as_ptr() as *const _, 0);
        let r = ffmpeg_next::ffi::avformat_open_input(&mut ictx, url.as_ptr(), std::ptr::null_mut(), &mut dict);
        ffmpeg_next::ffi::av_dict_free(&mut dict);
        assert_eq!(r, 0, "open failed");
        ffmpeg_next::ffi::avformat_find_stream_info(ictx, std::ptr::null_mut());
        let mut pkt = ffmpeg_next::ffi::av_packet_alloc();
        let t0 = Instant::now();
        let mut last = Instant::now();
        let mut bytes: u64 = 0;
        while t0.elapsed() < Duration::from_secs(40) {
            if ictx.is_null() {
                // reopen (EOF / error path: mirrors the backend's reconnect)
                let mut dict2: *mut ffmpeg_next::ffi::AVDictionary = std::ptr::null_mut();
                ffmpeg_next::ffi::av_dict_set(&mut dict2, b"rtsp_transport\0".as_ptr() as *const _, b"tcp\0".as_ptr() as *const _, 0);
                let r = ffmpeg_next::ffi::avformat_open_input(&mut ictx, url.as_ptr(), std::ptr::null_mut(), &mut dict2);
                ffmpeg_next::ffi::av_dict_free(&mut dict2);
                if r != 0 { println!("open failed {r}"); break; }
                ffmpeg_next::ffi::avformat_find_stream_info(ictx, std::ptr::null_mut());
            }
            let r = ffmpeg_next::ffi::av_read_frame(ictx, pkt);
            if r != 0 {
                ffmpeg_next::ffi::avformat_close_input(&mut ictx);
                continue;
            }
            bytes += (*pkt).size as u64;
            if last.elapsed() >= Duration::from_secs(5) {
                last = Instant::now();
                println!("[ffi-raw] {:+?} mem={:.0}MB read={:.1}MB", t0.elapsed(), mem_mb(), bytes as f64 / 1048576.0);
            }
        }
        ffmpeg_next::ffi::av_packet_free(&mut pkt);
        ffmpeg_next::ffi::avformat_close_input(&mut ictx);
        println!("[ffi-raw] DONE read={}MB", bytes / 1048576);
    }
}

/// avutil allocator sanity: av_buffer_alloc+unref and av_malloc+av_free loops.
#[test]
#[ignore = "allocator probe"]
fn leak_ffi_alloc() {
    #[cfg(feature = "ffmpeg")]
    unsafe {
        // 1) av_malloc / av_free
        let t0 = Instant::now();
        let mut last = Instant::now();
        let mut i: u64 = 0;
        while t0.elapsed() < Duration::from_secs(10) {
            let p = ffmpeg_next::ffi::av_malloc(65536) as *mut u8;
            assert!(!p.is_null());
            ffmpeg_next::ffi::av_free(p as *mut _);
            i += 1;
            if last.elapsed() >= Duration::from_secs(2) {
                last = Instant::now();
                println!("[alloc] malloc/free {:+?} mem={:.0}MB iters={i}", t0.elapsed(), mem_mb());
            }
        }
        // 2) av_buffer_alloc + av_buffer_unref (the packet-data path)
        while t0.elapsed() < Duration::from_secs(20) {
            let mut buf = ffmpeg_next::ffi::av_buffer_alloc(65536);
            assert!(!buf.is_null());
            ffmpeg_next::ffi::av_buffer_unref(&mut buf);
            i += 1;
            if last.elapsed() >= Duration::from_secs(2) {
                last = Instant::now();
                println!("[alloc] buffer/unref {:+?} mem={:.0}MB iters={i}", t0.elapsed(), mem_mb());
            }
        }
        println!("[alloc] DONE iters={i}");
    }
}

#[test]
#[ignore = "hls variant"]
fn leak_hls() {
    run("hls", "http://127.0.0.1:8888/cam/index.m3u8", 40);
}

#[test]
#[ignore = "reconnect loop against dead port"]
fn leak_reconnecting() {
    run("reconnecting", "rtsp://127.0.0.1:9999/nope", 40);
}
