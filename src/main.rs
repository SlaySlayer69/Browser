//! CleanDark — a minimal, resource-frugal browser on the WebView2 runtime.
//!
//! Run with `--incognito` for a private window. Private windows are separate
//! processes on purpose: the profile lives in memory, so closing the window
//! returns every byte of it to the OS at once.

// A GUI process should not flash a console window. Debug builds keep the
// console so panics and `dbg!` stay visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// On a non-Windows host the browser shell is compiled out, so the portable
// core's API looks unused. It is not — it is exercised by `cargo test` here and
// consumed by `browser::App` on Windows, which is where the real dead-code
// check happens.
#![cfg_attr(not(windows), allow(dead_code))]

mod blocker;
mod browser;
mod config;
mod engine;
mod ipc;
#[cfg(windows)]
mod platform;
mod stats;
mod storage;
mod util;
mod vault;

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    let incognito = std::env::args().any(|arg| arg == "--incognito");

    unsafe {
        // WebView2 needs an STA on the thread owning the controllers.
        // `CoInitializeEx` returns S_FALSE when the apartment already exists,
        // which is not an error for us.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    platform::window::init_process()?;

    let asset_dir = asset_dir();
    if !asset_dir.join("chrome.html").exists() {
        // Without the UI folder the chrome WebView would load a blank page and
        // the window would just look broken, with nothing explaining why.
        show_fatal_error(&format!(
            "The UI folder was not found.\n\nExpected: {}\n\nCopy the `ui` directory next to the \
             executable.",
            asset_dir.display()
        ));
        return Ok(());
    }

    // The app is kept alive across the message loop; dropping it here would
    // tear the browser down before the first message is pumped.
    let app = match browser::App::launch(incognito, asset_dir) {
        Ok(app) => app,
        Err(error) => {
            show_fatal_error(&format!(
                "CleanDark could not start.\n\n{error}\n\nThe WebView2 runtime is required. \
                 Install it from https://developer.microsoft.com/microsoft-edge/webview2/"
            ));
            return Ok(());
        }
    };

    platform::window::run_message_loop();
    drop(app);
    Ok(())
}

/// The UI ships in a `ui` folder next to the executable.
#[cfg(windows)]
fn asset_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ui")))
        .unwrap_or_else(|| std::path::PathBuf::from("ui"))
}

#[cfg(windows)]
fn show_fatal_error(message: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            &HSTRING::from("CleanDark"),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn main() {
    // The portable core (blocker, storage, vault, config, IPC) builds and its
    // tests run on any host; only the engine binding is Windows-only.
    eprintln!(
        "CleanDark's browser shell requires Windows and the WebView2 runtime.\n\
         The portable core still builds here — run `cargo test` to exercise it."
    );
    std::process::exit(1);
}
