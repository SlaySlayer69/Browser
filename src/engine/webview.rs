//! Per-WebView configuration and the memory levers.

use std::sync::mpsc;

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{AddScriptToExecuteOnDocumentCreatedCompletedHandler, TrySuspendCompletedHandler};
use windows::core::{Interface, Result, HSTRING};

use crate::config::Settings;

/// Which of the two roles a WebView plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The HTML browser chrome: tab strip, toolbar, menus.
    Chrome,
    /// A tab's page content.
    Content,
}

/// Turn off everything the browser does not use.
///
/// Most of these are not just cosmetic: autofill, password autosave and
/// SmartScreen each keep state and, in the first two cases, a service
/// connection alive per profile.
pub fn harden(webview: &ICoreWebView2, role: Role, settings: &Settings) -> Result<()> {
    unsafe {
        let s = webview.Settings()?;

        s.SetIsStatusBarEnabled(false)?;
        s.SetIsZoomControlEnabled(role == Role::Content)?;
        // Only the chrome may talk to the host, and it does so through the
        // typed command protocol. Host objects are never exposed.
        s.SetAreHostObjectsAllowed(false)?;
        s.SetIsWebMessageEnabled(true)?;
        s.SetIsBuiltInErrorPageEnabled(true)?;
        s.SetAreDevToolsEnabled(cfg!(debug_assertions))?;
        // The chrome has its own context menu; a default one over the tab strip
        // would be wrong. Pages keep theirs.
        s.SetAreDefaultContextMenusEnabled(role == Role::Content)?;

        if let Ok(s3) = s.cast::<ICoreWebView2Settings3>() {
            // The chrome owns the shortcuts (Ctrl+T, Ctrl+W, ...); letting
            // WebView2 also act on them would double-handle every key.
            s3.SetAreBrowserAcceleratorKeysEnabled(role == Role::Content)?;
        }
        if let Ok(s5) = s.cast::<ICoreWebView2Settings5>() {
            s5.SetIsPinchZoomEnabled(role == Role::Content)?;
        }
        if let Ok(s6) = s.cast::<ICoreWebView2Settings6>() {
            // Swipe-to-navigate inside the chrome would fire history
            // navigation on the wrong WebView.
            s6.SetIsSwipeNavigationEnabled(false)?;
        }
        if let Ok(s8) = s.cast::<ICoreWebView2Settings8>() {
            s8.SetIsReputationCheckingRequired(!settings.disable_smartscreen)?;
        }
        if role == Role::Chrome {
            if let Ok(s9) = s.cast::<ICoreWebView2Settings9>() {
                // Lets the HTML header declare its own drag regions with
                // `app-region: drag`, so the host never has to guess which
                // pixels move the window.
                s9.SetIsNonClientRegionSupportEnabled(true)?;
            }
        }
    }
    Ok(())
}

/// Disable the profile-level features we replace with our own.
///
/// Best-effort: on runtimes older than 1.0.1938.49 the interface is missing and
/// the built-in password manager simply stays as WebView2 configured it.
pub fn harden_profile(webview: &ICoreWebView2) -> Result<()> {
    unsafe {
        let Ok(webview13) = webview.cast::<ICoreWebView2_13>() else {
            return Ok(());
        };
        let profile = webview13.Profile()?;
        if let Ok(profile6) = profile.cast::<ICoreWebView2Profile6>() {
            // We ship a vault; the built-in one would prompt over ours and
            // store credentials somewhere we do not control.
            profile6.SetIsPasswordAutosaveEnabled(false)?;
            profile6.SetIsGeneralAutofillEnabled(false)?;
        }
    }
    Ok(())
}

/// Register a script that runs before any page script on every document.
pub fn add_startup_script(webview: &ICoreWebView2, script: &str) -> Result<()> {
    let script = HSTRING::from(script);
    let webview = webview.clone();
    let (tx, rx) = mpsc::channel();

    AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            webview
                .AddScriptToExecuteOnDocumentCreated(&script, &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, _id| {
            let _ = tx.send(code);
            Ok(())
        }),
    )
    .map_err(|e| windows::core::Error::new(windows::core::HRESULT(-1), format!("{e:?}")))?;

    rx.recv()
        .map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?
}

/// Ask the renderer to trim its caches. Cheap and instantly reversible.
pub fn set_low_memory(webview: &ICoreWebView2, low: bool) -> Result<()> {
    unsafe {
        let Ok(webview19) = webview.cast::<ICoreWebView2_19>() else {
            return Ok(());
        };
        webview19.SetMemoryUsageTargetLevel(if low {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
        } else {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
        })
    }
}

/// Freeze a background tab.
///
/// WebView2 refuses to suspend a visible WebView, so the caller must have
/// hidden the controller first.
///
/// Deliberately asynchronous: the only caller is the reclaim timer, and
/// blocking there on `wait_for_async_operation` would re-enter the message
/// pump from inside a `WM_TIMER` handler. `on_done` receives whether the freeze
/// actually took — a tab we wrongly believe is suspended would never be
/// retried, so the result is worth recording.
pub fn try_suspend<F>(webview: &ICoreWebView2, on_done: F) -> Result<()>
where
    F: FnOnce(bool) + 'static,
{
    let Ok(webview3) = webview.cast::<ICoreWebView2_3>() else {
        on_done(false);
        return Ok(());
    };

    let mut on_done = Some(on_done);
    let handler = TrySuspendCompletedHandler::create(Box::new(move |code, succeeded| {
        if let Some(callback) = on_done.take() {
            callback(code.is_ok() && succeeded);
        }
        Ok(())
    }));

    unsafe { webview3.TrySuspend(&handler) }
}

/// Unfreeze a suspended tab. Safe to call on a tab that is not suspended.
pub fn resume(webview: &ICoreWebView2) -> Result<()> {
    unsafe {
        let Ok(webview3) = webview.cast::<ICoreWebView2_3>() else {
            return Ok(());
        };
        webview3.Resume()
    }
}

/// Map a folder to a virtual https origin so the UI can be loaded without a
/// custom scheme handler.
///
/// This is the cheapest way to serve our own pages: WebView2 reads the files
/// directly, so there is no per-request trip into our process the way a custom
/// scheme or a `WebResourceRequested` handler would need.
pub fn map_asset_folder(webview: &ICoreWebView2, host_name: &str, folder: &std::path::Path) -> Result<()> {
    unsafe {
        let Ok(webview3) = webview.cast::<ICoreWebView2_3>() else {
            return Err(windows::core::Error::new(
                windows::core::HRESULT(-1),
                "WebView2 runtime is too old to map asset folders",
            ));
        };
        webview3.SetVirtualHostNameToFolderMapping(
            &HSTRING::from(host_name),
            &HSTRING::from(folder.as_os_str()),
            // Our pages are same-origin with each other and must not be
            // reachable as a cross-origin resource from the open web.
            COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_DENY_CORS,
        )
    }
}

pub fn navigate(webview: &ICoreWebView2, url: &str) -> Result<()> {
    unsafe { webview.Navigate(&HSTRING::from(url)) }
}

pub fn post_json(webview: &ICoreWebView2, json: &str) -> Result<()> {
    unsafe { webview.PostWebMessageAsJson(&HSTRING::from(json)) }
}
