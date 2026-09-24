//! Crash handling: unhandled SEH exceptions → minidump in
//! `%APPDATA%\PlayPlugin\crash\`, plus a panic hook that logs the panic.

use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Diagnostics::Debug::{
    MiniDumpWriteDump, SetUnhandledExceptionFilter, MINIDUMP_EXCEPTION_INFORMATION, MINIDUMP_TYPE,
};
use windows::Win32::System::Threading::GetCurrentProcessId;

static CRASH_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
static CRASHED: AtomicBool = AtomicBool::new(false);

pub fn install(data_dir: &std::path::Path) -> anyhow::Result<()> {
    let dir = data_dir.join("crash");
    std::fs::create_dir_all(&dir)?;
    let _ = CRASH_DIR.set(dir);
    unsafe {
        SetUnhandledExceptionFilter(Some(top_level_filter));
    }
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        CRASHED.store(true, Ordering::SeqCst);
        tracing::error!(panic = %info, "panic");
        default_panic(info);
    }));
    Ok(())
}

unsafe extern "system" fn top_level_filter(
    exception_pointers: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    // Do not dump on normal panic unwinds (panic hook logs those).
    if CRASHED.load(Ordering::SeqCst) {
        return 1; // EXCEPTION_EXECUTE_HANDLER → terminate
    }
    write_dump(exception_pointers);
    1
}

unsafe fn write_dump(
    exception_pointers: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) {
    let Some(dir) = CRASH_DIR.get() else { return };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("play-plugin-{ts}.dmp"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let mdei = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: windows::Win32::System::Threading::GetCurrentThreadId(),
        ExceptionPointers: exception_pointers as *mut _,
        ClientPointers: false.into(),
    };
    let handle: HANDLE = HANDLE(file.as_raw_handle());
    let _ = MiniDumpWriteDump(
        handle,
        GetCurrentProcessId(),
        HANDLE(file.as_raw_handle()),
        MINIDUMP_TYPE(0),
        Some(&mdei),
        None,
        None,
    );
}
