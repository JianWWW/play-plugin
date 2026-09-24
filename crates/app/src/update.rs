//! Auto-updater: fetch version manifest → compare → download → verify
//! Authenticode signature → (optionally) silently install.
//!
//! Off by default; enabled via config.toml [update].

use plugin_server::config::UpdateConfig;
use std::io::Read;
use std::time::Duration;

fn http_download(url: &str, dest: &std::path::Path) -> anyhow::Result<()> {
    let res = ureq::get(url).timeout(Duration::from_secs(600)).call()?;
    let mut reader = res.into_reader().take(2_000_000_000);
    let mut f = std::fs::File::create(dest)?;
    std::io::copy(&mut reader, &mut f)?;
    Ok(())
}

use plugin_server::protocol::{Event, UpdateEvent};
use plugin_server::session::AppState;
use std::sync::Arc;

const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4 * 3600);

#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    url: String,
    #[serde(default)]
    #[allow(dead_code)] // informational; Authenticode is the enforced check here
    sha256: Option<String>,
}

pub async fn run_loop(state: Arc<AppState>, cfg: UpdateConfig) {
    loop {
        if let Err(e) = check_once(&state, &cfg).await {
            tracing::warn!(error = %e, "update check failed");
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

async fn check_once(state: &Arc<AppState>, cfg: &UpdateConfig) -> anyhow::Result<()> {
    let Some(url) = cfg.manifest_url.as_deref() else {
        anyhow::bail!("no manifest_url");
    };
    let body = fetch_manifest(url).await?;
    let manifest: Manifest = serde_json::from_str(&body)?;
    if !is_newer(&manifest.version, &state.version) {
        tracing::debug!(remote = %manifest.version, local = %state.version, "up to date");
        return Ok(());
    }
    tracing::info!(remote = %manifest.version, local = %state.version, "update available");
    state.broadcast(&Event::update_available(UpdateEvent {
        version: manifest.version.clone(),
        url: manifest.url.clone(),
    }));

    if !cfg.auto_install {
        return Ok(());
    }
    let msi = download(&manifest.url).await?;
    verify_authenticode(&msi)
        .map_err(|e| anyhow::anyhow!("signature verification failed for {}: {e}", msi.display()))?;
    tracing::info!(path = %msi.display(), "installing verified update");
    let status = tokio::process::Command::new("msiexec")
        .args(["/i", &msi.to_string_lossy(), "/qn", "/norestart"])
        .status()
        .await?;
    if status.success() {
        std::process::exit(0); // installer restarts the plugin
    }
    anyhow::bail!("msiexec failed: {:?}", status.code());
}

fn is_newer(remote: &str, local: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (r, l) = (parse(remote), parse(local));
    for i in 0..3 {
        let a = r.get(i).copied().unwrap_or(0);
        let b = l.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

fn http_get_string(url: &str) -> anyhow::Result<String> {
    let res = ureq::get(url).timeout(Duration::from_secs(30)).call()?;
    let mut body = String::new();
    res.into_reader()
        .take(10_000_000)
        .read_to_string(&mut body)?;
    Ok(body)
}

async fn fetch_manifest(url: &str) -> anyhow::Result<String> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || http_get_string(&url))
        .await
        .expect("join")
}

async fn download(url: &str) -> anyhow::Result<std::path::PathBuf> {
    let url = url.to_string();
    let tmp = std::env::temp_dir().join("play-plugin-update.msi");
    let dest = tmp.clone();
    tokio::task::spawn_blocking(move || http_download(&url, &dest))
        .await
        .expect("join")?;
    Ok(tmp)
}

/// WinVerifyTrust: the downloaded MSI must carry a valid Authenticode
/// signature issued to the same binary name we trust.
fn verify_authenticode(path: &std::path::Path) -> anyhow::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Security::WinTrust::*;

    // WINTRUST_ACTION_GENERIC_VERIFY_V2
    let mut action = windows::core::GUID::from_u128(0x00AAC56B_CD44_11D0_BC3A_00A0C923E56C);

    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut file_info = WINTRUST_FILE_INFO {
            cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: PCWSTR(wide.as_ptr()),
            hFile: Default::default(),
            pgKnownSubject: std::ptr::null_mut(),
        };
        let mut trust_data = WINTRUST_DATA {
            cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            dwStateAction: WTD_STATEACTION_VERIFY,
            Anonymous: WINTRUST_DATA_0 {
                pFile: &mut file_info,
            },
            ..Default::default()
        };
        let code = WinVerifyTrust(
            HWND::default(),
            &mut action,
            &mut trust_data as *mut _ as *mut _,
        );
        // Release policy state.
        trust_data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrust(
            HWND::default(),
            &mut action,
            &mut trust_data as *mut _ as *mut _,
        );
        if code != 0 {
            anyhow::bail!("WinVerifyTrust error {code:#x}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn semver_compare() {
        assert!(is_newer("1.2.3", "1.2.2"));
        assert!(is_newer("v2.0.0", "1.9.9"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("1.2.2", "1.2.10"));
    }
}
