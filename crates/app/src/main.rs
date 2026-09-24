//! play-plugin.exe — plugin process entry point.

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
        .filter(|p| p[0] + p[1] + p[2] > 60)
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
    std::fs::create_dir_all(data_dir.join("logs"))?;
    let (writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(
        data_dir.join("logs"),
        "play-plugin.log",
    ));
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(guard)
}
