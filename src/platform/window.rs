//! The Win32 host window.
//!
//! # Why a custom frame
//!
//! The design puts the tab strip where the title bar would be. That needs the
//! client area extended over the caption (`WM_NCCALCSIZE`), which in turn means
//! we own hit-testing for the resize borders. Dragging is *not* handled here:
//! WebView2's non-client region support lets the HTML chrome mark its own drag
//! areas with `app-region: drag`, so the host never has to guess which pixels
//! of the header are a button and which are empty space.
//!
//! # Layout
//!
//! Each WebView2 controller gets its own child HWND rather than sharing the
//! main window. That buys two things: z-order we control with `SetWindowPos`
//! (needed so the omnibox dropdown can overlay the page), and tab switching
//! that is a `ShowWindow` call instead of a resize.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Logical height of the chrome strip (tab bar + toolbar) at 96 DPI.
pub const CHROME_HEIGHT_DIP: i32 = 76;

/// Base background. Matches `--surface-0` in the stylesheet so a resize never
/// flashes a lighter colour before the WebView paints.
const SURFACE_0: u32 = 0x0011_0F0E; // COLORREF is 0x00BBGGRR

/// Messages the app layer handles. Kept above `WM_APP` so they cannot collide
/// with anything Windows sends.
pub const WM_APP_FILTERS_READY: u32 = WM_APP + 1;
/// Posted by the browser layer when chrome state has changed. See
/// [`crate::browser::pending`] for why updates are coalesced through a message
/// rather than sent as they happen.
pub const WM_APP_FLUSH_UI: u32 = WM_APP + 2;

/// Shortest gap between WebView resizes while the user is dragging the frame.
///
/// Windows sends `WM_SIZE` continuously during a drag, and each one costs a
/// renderer relayout. One per frame is all a 60 Hz display can show; the exact
/// final size is applied on `WM_EXITSIZEMOVE`.
const RESIZE_THROTTLE: Duration = Duration::from_millis(16);

/// Timer id for the background-tab reclaim policy.
pub const TIMER_RECLAIM: usize = 1;

/// Callbacks the window forwards to the browser layer. Implemented by
/// [`crate::browser::App`]; kept as a trait so this module has no dependency on
/// the browser's internals.
pub trait WindowDelegate {
    /// Client area changed; re-layout the WebView controllers.
    fn on_resize(&self, width: i32, height: i32, dpi: u32);
    /// Coalesced chrome update is due.
    fn on_flush_ui(&self);
    /// The window was minimized or restored. A minimized window paints
    /// nothing, which makes even its foreground tab reclaimable.
    fn on_minimized(&self, minimized: bool);
    /// Reclaim timer fired.
    fn on_reclaim_tick(&self);
    /// Freshly compiled filter lists are ready to be swapped in.
    fn on_filters_ready(&self);
    /// Maximized state changed; the UI mirrors it in the window buttons.
    fn on_maximize_changed(&self, maximized: bool);
    /// The user is closing the window. Return `true` to allow it.
    fn on_close(&self) -> bool;
}

/// Per-window state reachable from the window procedure.
struct WindowState {
    delegate: RefCell<Option<Rc<dyn WindowDelegate>>>,
    was_maximized: Cell<bool>,
    was_minimized: Cell<bool>,
    /// True between WM_ENTERSIZEMOVE and WM_EXITSIZEMOVE, i.e. while the user
    /// is dragging the frame. Only then is resizing throttled.
    in_size_move: Cell<bool>,
    last_resize: Cell<Instant>,
}

pub struct MainWindow {
    hwnd: HWND,
    /// Host for the HTML chrome. Always on top of the content hosts.
    chrome_host: HWND,
    state: *mut WindowState,
}

/// Process-wide setup. Must run before any window or WebView2 call.
pub fn init_process() -> Result<()> {
    unsafe {
        // Per-monitor v2: the window is told about DPI changes and Windows
        // stops bitmap-stretching our chrome across monitors.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    Ok(())
}

impl MainWindow {
    pub fn create(title: &str, incognito: bool) -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class_name = w!("CleanDarkMainWindow");

            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH(CreateSolidBrush(COLORREF(SURFACE_0)).0),
                lpszClassName: class_name,
                ..Default::default()
            };
            // A duplicate registration is fine: a second window reuses the
            // class. Any other failure surfaces on CreateWindowExW.
            RegisterClassExW(&class);

            let state = Box::into_raw(Box::new(WindowState {
                delegate: RefCell::new(None),
                was_maximized: Cell::new(false),
                was_minimized: Cell::new(false),
                in_size_move: Cell::new(false),
                last_resize: Cell::new(Instant::now()),
            }));

            let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1200,
                800,
                None,
                None,
                Some(instance.into()),
                Some(state as *const _),
            )?;

            // Dark title bar and border. These fail harmlessly on Windows 10
            // builds older than 1809, where the light frame is the only option.
            let dark = windows::Win32::Foundation::TRUE;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const _ as *const _,
                std::mem::size_of_val(&dark) as u32,
            );
            // Incognito gets a violet border so the window is identifiable at a
            // glance; normal windows get a border matching the chrome.
            let border = COLORREF(if incognito { 0x00FF_5C8A } else { SURFACE_0 });
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &border as *const _ as *const _,
                std::mem::size_of_val(&border) as u32,
            );

            // Force a WM_NCCALCSIZE round so the custom frame takes effect.
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;

            let chrome_host = create_host_window(hwnd, instance.into())?;

            Ok(Self { hwnd, chrome_host, state })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn chrome_host(&self) -> HWND {
        self.chrome_host
    }

    /// Create an additional child host, e.g. for a tab's content WebView.
    pub fn create_content_host(&self) -> Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let host = create_host_window(self.hwnd, instance.into())?;
            // Content sits *below* the chrome host so the omnibox dropdown can
            // overlay it.
            SetWindowPos(
                host,
                Some(HWND_BOTTOM),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )?;
            Ok(host)
        }
    }

    pub fn set_delegate(&self, delegate: Rc<dyn WindowDelegate>) {
        unsafe {
            *(*self.state).delegate.borrow_mut() = Some(delegate);
        }
    }

    pub fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
        }
    }

    pub fn dpi(&self) -> u32 {
        unsafe { GetDpiForWindow(self.hwnd).max(96) }
    }

    /// Client size in physical pixels.
    pub fn client_size(&self) -> (i32, i32) {
        unsafe {
            let mut rect = RECT::default();
            if GetClientRect(self.hwnd, &mut rect).is_err() {
                return (0, 0);
            }
            (rect.right - rect.left, rect.bottom - rect.top)
        }
    }

    /// Chrome strip height in physical pixels for the current DPI.
    pub fn chrome_height(&self) -> i32 {
        scale(CHROME_HEIGHT_DIP, self.dpi())
    }

    pub fn is_maximized(&self) -> bool {
        unsafe { IsZoomed(self.hwnd).as_bool() }
    }

    pub fn toggle_maximize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, if self.is_maximized() { SW_RESTORE } else { SW_MAXIMIZE });
        }
    }

    pub fn minimize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
        }
    }

    pub fn close(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// Position a child host in client coordinates.
    pub fn place_host(&self, host: HWND, rect: RECT, visible: bool) {
        unsafe {
            let _ = SetWindowPos(
                host,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let _ = ShowWindow(host, if visible { SW_SHOWNA } else { SW_HIDE });
        }
    }

    /// Hide a child host without touching its position.
    ///
    /// Used for background tabs during layout: `ShowWindow(SW_HIDE)` on an
    /// already-hidden window is a no-op inside Windows, whereas `SetWindowPos`
    /// is not.
    pub fn hide_host(&self, host: HWND) {
        unsafe {
            let _ = ShowWindow(host, SW_HIDE);
        }
    }

    /// Start (or restart) the reclaim timer. `interval_ms == 0` stops it.
    pub fn set_reclaim_timer(&self, interval_ms: u32) {
        unsafe {
            if interval_ms == 0 {
                let _ = KillTimer(Some(self.hwnd), TIMER_RECLAIM);
            } else {
                SetTimer(Some(self.hwnd), TIMER_RECLAIM, interval_ms, None);
            }
        }
    }

    /// Wake the message loop from another thread.
    pub fn post_app_message(hwnd: HWND, message: u32) {
        unsafe {
            let _ = PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for MainWindow {
    fn drop(&mut self) {
        // The state box is owned by the window and released in WM_NCDESTROY.
    }
}

/// `DefWindowProcW` is declared as a plain `unsafe fn`, so it cannot be used
/// directly as a `WNDPROC`. This thunk gives it the `extern "system"` ABI the
/// window class expects.
unsafe extern "system" fn host_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The host window is a bare container for a WebView2 controller; it has no
    // painting of its own, so erasing would only cause a flash on resize.
    if message == WM_ERASEBKGND {
        return LRESULT(1);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe fn create_host_window(parent: HWND, instance: windows::Win32::Foundation::HINSTANCE) -> Result<HWND> {
    let class_name = w!("CleanDarkHost");
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(host_wnd_proc),
        hInstance: instance,
        hbrBackground: HBRUSH(CreateSolidBrush(COLORREF(SURFACE_0)).0),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassExW(&class);

    CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        PCWSTR::null(),
        WS_CHILD | WS_CLIPSIBLINGS,
        0,
        0,
        0,
        0,
        Some(parent),
        None,
        Some(instance),
        None,
    )
}

/// Scale a 96-DPI logical value to physical pixels.
pub fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // WM_NCCREATE carries the state pointer we passed to CreateWindowExW.
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        let state = (*create).lpCreateParams as *mut WindowState;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
    if state.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }

    let delegate = || (*state).delegate.borrow().clone();

    match message {
        // Extend the client area over the caption while keeping the resize
        // borders. Returning 0 with an adjusted rect is what removes the
        // title bar without losing snap, shadows or the maximize animation.
        WM_NCCALCSIZE if wparam.0 == 1 => {
            let params = lparam.0 as *mut NCCALCSIZE_PARAMS;
            let rect = &mut (*params).rgrc[0];
            let dpi = GetDpiForWindow(hwnd).max(96);

            let frame_x = GetSystemMetricsForDpi(SM_CXFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            let frame_y = GetSystemMetricsForDpi(SM_CYFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);

            rect.left += frame_x;
            rect.right -= frame_x;
            rect.bottom -= frame_y;
            // `rect.top` is left alone, which is what pulls the client area up
            // into the caption. A maximized window, however, really does hang
            // over the screen edge by the frame thickness.
            if IsZoomed(hwnd).as_bool() {
                rect.top += frame_y;
            }
            LRESULT(0)
        }

        // Only the top resize border needs us; `app-region: drag` in the HTML
        // chrome covers moving the window.
        WM_NCHITTEST => {
            let default = DefWindowProcW(hwnd, message, wparam, lparam);
            if default.0 != HTCLIENT as isize {
                return default;
            }
            let dpi = GetDpiForWindow(hwnd).max(96);
            let border = GetSystemMetricsForDpi(SM_CYFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);

            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut window_rect = RECT::default();
            if GetWindowRect(hwnd, &mut window_rect).is_ok()
                && !IsZoomed(hwnd).as_bool()
                && y < window_rect.top + border
            {
                return LRESULT(HTTOP as isize);
            }
            default
        }

        WM_ENTERSIZEMOVE => {
            (*state).in_size_move.set(true);
            LRESULT(0)
        }

        WM_EXITSIZEMOVE => {
            (*state).in_size_move.set(false);
            // The drag may have ended on a size the throttle skipped, so the
            // final geometry is always applied exactly once here.
            if let Some(delegate) = delegate() {
                let mut rect = RECT::default();
                if GetClientRect(hwnd, &mut rect).is_ok() {
                    delegate.on_resize(
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        GetDpiForWindow(hwnd).max(96),
                    );
                }
            }
            LRESULT(0)
        }

        WM_SIZE => {
            let Some(delegate) = delegate() else { return LRESULT(0) };

            let minimized = wparam.0 as u32 == SIZE_MINIMIZED;
            if (*state).was_minimized.replace(minimized) != minimized {
                delegate.on_minimized(minimized);
            }
            if minimized {
                // A minimized window has a zero-sized client area; laying the
                // WebViews out to it would only have to be undone on restore.
                return LRESULT(0);
            }

            let maximized = IsZoomed(hwnd).as_bool();
            if (*state).was_maximized.replace(maximized) != maximized {
                delegate.on_maximize_changed(maximized);
            }

            // Throttled only while the frame is being dragged; a one-shot
            // resize (maximize, restore, DPI change) is applied immediately.
            let now = Instant::now();
            if (*state).in_size_move.get()
                && now.duration_since((*state).last_resize.get()) < RESIZE_THROTTLE
            {
                return LRESULT(0);
            }
            (*state).last_resize.set(now);

            let width = (lparam.0 & 0xFFFF) as i16 as i32;
            let height = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            delegate.on_resize(width, height, GetDpiForWindow(hwnd).max(96));
            LRESULT(0)
        }

        WM_DPICHANGED => {
            // lparam points at the suggested new window rect.
            let suggested = lparam.0 as *const RECT;
            let _ = SetWindowPos(
                hwnd,
                None,
                (*suggested).left,
                (*suggested).top,
                (*suggested).right - (*suggested).left,
                (*suggested).bottom - (*suggested).top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            LRESULT(0)
        }

        // The whole client area is covered by WebView controllers, so erasing
        // it would be pure overdraw and a visible flash while resizing.
        WM_ERASEBKGND => LRESULT(1),

        WM_TIMER if wparam.0 == TIMER_RECLAIM => {
            if let Some(delegate) = delegate() {
                delegate.on_reclaim_tick();
            }
            LRESULT(0)
        }

        WM_APP_FLUSH_UI => {
            if let Some(delegate) = delegate() {
                delegate.on_flush_ui();
            }
            LRESULT(0)
        }

        WM_APP_FILTERS_READY => {
            if let Some(delegate) = delegate() {
                delegate.on_filters_ready();
            }
            LRESULT(0)
        }

        WM_CLOSE => {
            let allow = delegate().map(|d| d.on_close()).unwrap_or(true);
            if allow {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(state));
            DefWindowProcW(hwnd, message, wparam, lparam)
        }

        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Standard message pump.
pub fn run_message_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scale;

    #[test]
    fn dpi_scaling_rounds_to_nearest() {
        assert_eq!(scale(76, 96), 76);
        assert_eq!(scale(76, 144), 114); // 150%
        assert_eq!(scale(76, 192), 152); // 200%
        assert_eq!(scale(10, 120), 13); // 125%, 12.5 rounds up
    }
}
