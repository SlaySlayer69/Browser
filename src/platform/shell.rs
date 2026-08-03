//! Shell integration for the download list: open a finished file, or reveal it
//! in Explorer.

use std::path::Path;

use windows::core::{w, Result, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open a downloaded file with its default handler.
///
/// The caller is responsible for only passing paths that came out of our own
/// downloads table — never a path chosen by page content.
pub fn open_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(windows::core::Error::new(
            windows::core::HRESULT(-1),
            "file no longer exists",
        ));
    }

    let path = wide(&path.to_string_lossy());
    unsafe {
        // ShellExecuteW returns a fake HINSTANCE; values <= 32 are errors.
        let result = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if result.0 as usize <= 32 {
            return Err(windows::core::Error::from_thread());
        }
    }
    Ok(())
}

/// Open Explorer with the file selected.
pub fn reveal_in_explorer(path: &Path) -> Result<()> {
    // `/select,` needs the path quoted so spaces do not split the argument.
    let args = wide(&format!("/select,\"{}\"", path.to_string_lossy()));
    unsafe {
        let result = ShellExecuteW(
            None,
            w!("open"),
            w!("explorer.exe"),
            PCWSTR(args.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if result.0 as usize <= 32 {
            return Err(windows::core::Error::from_thread());
        }
    }
    Ok(())
}
