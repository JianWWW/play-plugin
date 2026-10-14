//! play-plugin.exe — plugin process entry point.

// Release builds are GUI-subsystem (no console flash on login autostart);
// debug builds keep the console so `cargo run` shows logs directly.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod crash;
mod instance;
mod tray;
mod update;

use anyhow::Context;
use plugin_overlay::audio::AudioHub;
use plugin_overlay::host::OverlayHost;
use std::sync::Arc;
use tracing_appender::non_blocking::WorkerGuard;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut config_path = None;
    let mut smoke = false;
    let mut write_origins: Option<Vec<String>> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                config_path = args.get(i).map(std::path::PathBuf::from);
            }
            "--smoke" => smoke = true,
            "--write-config" => {
                // MSI custom action: --write-config --origins "https://a.com;https://b.com"
                if args.get(i + 1).map(String::as_str) == Some("--origins") {
                    i += 2;
                    write_origins = Some(
                        args.get(i)
                            .cloned()
                            .unwrap_or_default()
                            .split(';')
                            .map(|s| {
                                // MSI/custom-action quoting styles vary.
                                s.trim().trim_matches('"').trim().to_string()
                            })
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                }
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    // Per-monitor DPI awareness before any window is created.
    unsafe {
        use windows::Win32::UI::HiDpi::*;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let (mut config, data_dir) = plugin_server::config::Config::load(config_path);
    config.data_dir = Some(data_dir.clone());
    let _guard = init_logging(&data_dir, &config.log_level)?;

    if let Some(origins) = write_origins {
        config.origins = origins;
        let file = data_dir.join("config.toml");
        std::fs::write(&file, build_toml(&config)).context("write config.toml")?;
        tracing::info!(path = %file.display(), "config written (installer preflight)");
        return Ok(());
    }

    crash::install(&data_dir)?;
    instance::acquire(&data_dir)?;

    // Retention: drop logs/snapshots/crash files older than 7 days.
    cleanup_before(
        &data_dir,
        std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 86400),
    );

    if smoke {
        return smoke_test(config);
    }

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "PlayPlugin starting");

    let host = OverlayHost::start().context("overlay host")?;
    let audio = Arc::new(AudioHub::new());
    let state = plugin_server::session::AppState::new(
        config.clone(),
        host.clone(),
        audio.clone(),
        env!("CARGO_PKG_VERSION"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let port = rt.block_on(plugin_server::serve(config.clone(), state.clone()))?;

    // Auto-update loop (optional, off by default).
    if config.update.enabled && config.update.manifest_url.is_some() {
        let st = state.clone();
        let uc = config.update.clone();
        rt.spawn(async move {
            update::run_loop(st, uc).await;
        });
    }

    // Tray runs its own message pump; when the user exits it signals here.
    let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
    tray::spawn(port, data_dir.clone(), exit_tx.clone());

    // Also exit on Ctrl+C (running under a console during development).
    {
        let rt2 = rt.handle().clone();
        std::thread::spawn(move || {
            rt2.block_on(async {
                let _ = tokio::signal::ctrl_c().await;
            });
            let _ = exit_tx.send(());
        });
    }

    let _ = exit_rx.recv();
    audio.drop_all();
    tracing::info!("PlayPlugin exiting");
    Ok(())
}

fn smoke_test(_config: plugin_server::config::Config) -> anyhow::Result<()> {
    use plugin_core::PipelineEvent;
    println!("[smoke] starting overlay host…");
    let host = OverlayHost::start()?;
    println!("[smoke] host ready");

    let ids = [1u64, 2, 3, 4];
    for (n, &id) in ids.iter().enumerate() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = plugin_core::spawn_stream(
            plugin_core::StreamOptions {
                url: "test://pattern?w=960&h=540&fps=30".into(),
                muted: true,
                title: String::new(),
            },
            Some(host.gpu()),
            true,
            tx,
        );
        let h = host.clone();
        std::thread::spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                if let PipelineEvent::Video(f) = ev {
                    h.set_frame(id, f);
                }
            }
        });
        handle
            .close
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let gx = 200 + ((n % 2) * 980) as i32;
        let gy = 120 + ((n / 2) * 560) as i32;
        host.create(
            id,
            plugin_overlay::host::Rect {
                l: gx,
                t: gy,
                w: 960,
                h: 540,
            },
            plugin_overlay::host::UiConfig {
                title: String::new(),
                buttons: plugin_overlay::host::default_buttons(),
                speeds: vec![1.0, 2.0, 4.0, 8.0, 16.0],
            },
        );
    }
    println!("[smoke] 4 overlay windows requested; verifying…");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let n = host.window_count();
    assert_eq!(n, 4, "expected 4 overlay windows, got {n}");
    let frame = host.snapshot(ids[0]).expect("frame readback failed");
    // Content check: animated pattern = blue checkerboard + white moving bar.
    let px = frame.data.chunks_exact(4);
    let total = px.len();
    let nonblack = frame
        .data
        .chunks_exact(4)
        .filter(|p| u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2]) > 60)
        .count();
    let white = frame
        .data
        .chunks_exact(4)
        .filter(|p| p[0] > 240 && p[1] > 240 && p[2] > 240)
        .count();
    assert!(
        nonblack * 10 > total,
        "frame looks empty ({nonblack}/{total})"
    );
    assert!(white > 100, "white bar missing ({white} px)");
    println!("[smoke] frame content OK ({nonblack}/{total} non-black, {white} white-bar px)");
    println!("[smoke] 4 windows alive, frame readback OK; showing for 3 more seconds…");
    std::thread::sleep(std::time::Duration::from_secs(3));
    for &id in &ids {
        host.close(id);
    }
    println!("[smoke] OK");
    Ok(())
}

/// Deletes files under logs/, snapshots/ and crash/ modified before `cutoff`.
fn cleanup_before(data_dir: &std::path::Path, cutoff: std::time::SystemTime) {
    for sub in ["logs", "snapshots", "crash"] {
        let Ok(entries) = std::fs::read_dir(data_dir.join(sub)) else { continue };
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            let old = md.is_file() && md.modified().map(|t| t < cutoff).unwrap_or(false);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn build_toml(config: &plugin_server::config::Config) -> String {
    let origins = config
        .origins
        .iter()
        .map(|o| format!("\"{o}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "port = {}\nport_fallback = {}\nmax_streams = {}\nlog_level = \"{}\"\norigins = [{}]\n\n[update]\nenabled = {}\n",
        config.port, config.port_fallback, config.max_streams, config.log_level, origins, config.update.enabled
    )
}

fn init_logging(data_dir: &std::path::Path, level: &str) -> anyhow::Result<WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    std::fs::create_dir_all(data_dir.join("logs"))?;
    let (writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(
        data_dir.join("logs"),
        "play-plugin.log",
    ));
    // RUST_LOG beats config.toml so `RUST_LOG=debug` just works while debugging.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(level))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Reloadable so pages can raise the level at runtime via hello{debug:true}
    // (raise-only, plugin_server::logging holds the handle — ADR-0001).
    let (filter_layer, handle) = tracing_subscriber::reload::Layer::new(filter);
    plugin_server::logging::install(handle);
    // Console mirror for debugging: always in debug builds, opt-in otherwise.
    let console = cfg!(debug_assertions) || std::env::var_os("PLAY_PLUGIN_CONSOLE").is_some();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer);
    if console {
        tracing_subscriber::registry()
            .with(filter_layer)
            .with(file_layer)
            .with(tracing_subscriber::fmt::layer())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter_layer)
            .with(file_layer)
            .init();
    }
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_old_files() {
        let dir = std::env::temp_dir().join(format!("pp-clean-{}", std::process::id()));
        let log = dir.join("logs/play-plugin.log.2020-01-01");
        let snap = dir.join("snapshots/stream-1-0.png");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::create_dir_all(snap.parent().unwrap()).unwrap();
        std::fs::write(&log, b"x").unwrap();
        std::fs::write(&snap, b"x").unwrap();
        // Future cutoff: everything is "old" → both deleted.
        cleanup_before(&dir, std::time::SystemTime::now() + std::time::Duration::from_secs(1));
        assert!(!log.exists() && !snap.exists());
        // Epoch cutoff: nothing is old → file survives.
        std::fs::write(&log, b"x").unwrap();
        cleanup_before(&dir, std::time::SystemTime::UNIX_EPOCH);
        assert!(log.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
