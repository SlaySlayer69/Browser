//! Creating and configuring the WebView2 environment.

use std::path::Path;
use std::sync::mpsc;

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::core::{Interface, Result, HRESULT, HSTRING};
use windows::Win32::Foundation::{E_POINTER, HWND};

use crate::config::Settings;
use crate::engine::flags;

/// A shared WebView2 environment.
///
/// One environment per profile means one browser process shared by every tab,
/// which is where most of the memory advantage over a bundled Chromium comes
/// from: renderers are shared per site, and the browser, GPU and network
/// services exist exactly once.
pub struct Environment {
    inner: ICoreWebView2Environment,
}

impl Environment {
    /// Create the environment. Blocks the calling (UI) thread while pumping
    /// messages, because nothing can be drawn before this returns anyway.
    pub fn create(user_data_folder: &Path, settings: &Settings) -> Result<Self> {
        let options = CoreWebView2EnvironmentOptions::default();
        unsafe {
            options.set_additional_browser_arguments(flags::browser_arguments(settings));
            // The extension host is dead weight for a browser with no
            // extension UI.
            options.set_are_browser_extensions_enabled(false);
            // Edge's tracking prevention overlaps our own blocker and adds a
            // second per-request evaluation inside the browser process.
            options.set_enable_tracking_prevention(false);
            // Do not let WebView2 sign the user into an Edge profile.
            options.set_allow_single_sign_on_using_os_primary_account(false);
        }
        let options: ICoreWebView2EnvironmentOptions = options.into();

        let user_data = HSTRING::from(user_data_folder.as_os_str());
        let (tx, rx) = mpsc::channel();

        CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    windows::core::PCWSTR::null(),
                    &user_data,
                    &options,
                    &handler,
                )
                .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |code, environment| {
                code?;
                tx.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .map_err(|_| windows::core::Error::from(E_POINTER))?;
                Ok(())
            }),
        )
        .map_err(to_windows_error)?;

        let inner = rx.recv().map_err(|_| windows::core::Error::from(E_POINTER))??;
        Ok(Self { inner })
    }

    /// Create a controller hosted in `parent`.
    ///
    /// `incognito` maps to WebView2's in-private mode: the profile is created
    /// in memory and torn down with the last controller that uses it, so no
    /// cookie, cache entry or history row reaches the disk. It is a controller
    /// option rather than a second environment on purpose — a second
    /// environment over the same user-data folder would have to repeat the
    /// exact command line, and would spawn a second browser process.
    pub fn create_controller(
        &self,
        parent: HWND,
        incognito: bool,
    ) -> Result<ICoreWebView2Controller> {
        let (tx, rx) = mpsc::channel();

        // `CreateCoreWebView2ControllerWithOptions` needs Environment10
        // (WebView2 runtime 1.0.1661.34+). Without it we cannot honour an
        // incognito request, and silently opening a *recording* window would
        // be the worst possible failure, so that case is an error.
        let environment10: Option<ICoreWebView2Environment10> = self.inner.cast().ok();

        match (incognito, environment10) {
            (true, None) => {
                return Err(windows::core::Error::new(
                    HRESULT(-1),
                    "private windows need WebView2 runtime 1.0.1661.34 or newer",
                ))
            }
            (_, Some(environment)) => {
                let options = unsafe { environment.CreateCoreWebView2ControllerOptions()? };
                unsafe { options.SetIsInPrivateModeEnabled(incognito)? };

                CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                    Box::new(move |handler| unsafe {
                        environment
                            .CreateCoreWebView2ControllerWithOptions(parent, &options, &handler)
                            .map_err(webview2_com::Error::WindowsError)
                    }),
                    Box::new(move |code, controller| {
                        code?;
                        tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                            .map_err(|_| windows::core::Error::from(E_POINTER))?;
                        Ok(())
                    }),
                )
                .map_err(to_windows_error)?;
            }
            (false, None) => {
                let environment = self.inner.clone();
                CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                    Box::new(move |handler| unsafe {
                        environment
                            .CreateCoreWebView2Controller(parent, &handler)
                            .map_err(webview2_com::Error::WindowsError)
                    }),
                    Box::new(move |code, controller| {
                        code?;
                        tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                            .map_err(|_| windows::core::Error::from(E_POINTER))?;
                        Ok(())
                    }),
                )
                .map_err(to_windows_error)?;
            }
        }

        let controller = rx.recv().map_err(|_| windows::core::Error::from(E_POINTER))??;

        unsafe {
            // Paint the WebView's own background in the chrome colour so a
            // resize never flashes white before the page paints.
            if let Ok(controller2) = controller.cast::<ICoreWebView2Controller2>() {
                controller2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                    A: 255,
                    R: 0x0E,
                    G: 0x0F,
                    B: 0x11,
                })?;
            }
        }

        Ok(controller)
    }

    /// The response handed back for a blocked request.
    ///
    /// An empty 403 rather than a failed request: a failure surfaces as a
    /// network error in the page's console and makes some scripts retry, while
    /// a clean short-circuit response ends the matter.
    pub fn blocked_response(&self) -> Result<ICoreWebView2WebResourceResponse> {
        unsafe {
            self.inner.CreateWebResourceResponse(
                None,
                403,
                &HSTRING::from("Blocked"),
                &HSTRING::from("Content-Length: 0\r\nX-CleanDark-Blocked: 1"),
            )
        }
    }
}

fn to_windows_error(error: webview2_com::Error) -> windows::core::Error {
    match error {
        webview2_com::Error::WindowsError(e) => e,
        other => windows::core::Error::new(HRESULT(-1), format!("{other:?}")),
    }
}
