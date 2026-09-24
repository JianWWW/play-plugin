//! Single-instance guard via a named mutex (per session).

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

pub fn acquire(_data_dir: &std::path::Path) -> anyhow::Result<()> {
    unsafe {
        let handle = CreateMutexW(None, false, w!("Local\\PlayPlugin.SingleInstance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            tracing::warn!("another PlayPlugin instance is already running");
            let _ = CloseHandle(handle);
            anyhow::bail!("already running");
        }
        // Keep the handle open for the process lifetime: it holds the mutex.
        Ok(())
    }
}
