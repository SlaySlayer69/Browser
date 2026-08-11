//! The browser itself: window, chrome WebView, tabs, and command dispatch.
//!
//! Everything runs on the UI thread. WebView2 is apartment-threaded and calls
//! every event handler there, so `RefCell` is the right tool and there is no
//! lock anywhere in the hot paths. Borrows are kept short because handlers can
//! re-enter — a `WebResourceRequested` callback can fire while we are inside
//! `push_tabs`, for instance.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Instant;

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, DocumentTitleChangedEventHandler, DownloadStartingEventHandler,
    HistoryChangedEventHandler, IsDocumentPlayingAudioChangedEventHandler,
    NavigationCompletedEventHandler, NavigationStartingEventHandler, NewWindowRequestedEventHandler,
    SourceChangedEventHandler, StateChangedEventHandler, WebMessageReceivedEventHandler,
    WebResourceRequestedEventHandler,
};
use windows::core::{Interface, Result, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};

use crate::blocker::{lists, Blocker};
use crate::browser::pending::PendingUi;
use crate::browser::reclaim::{self, ReclaimAction, TabPower, TabState};
use crate::browser::tab::Tab;
use crate::config::{Paths, ReclaimMode, Settings};
use crate::engine::environment::Environment;
use crate::engine::resource::{filter_type_for, ResourceKind};
use crate::engine::webview::{self as wv, Role};
use crate::ipc::{Command, Event, SettingsView, ToastKind, VaultState};
use crate::platform::procstats::{self, ProcessSampler};
use crate::platform::window::{
    self, MainWindow, WindowDelegate, WM_APP_FILTERS_READY, WM_APP_FLUSH_UI,
};
use crate::stats::{self, PendingCounters, PrivacyStats};
use crate::storage::{counters, DownloadState, Storage};
use crate::util;
use crate::vault::{Vault, VaultError};

/// Virtual host the bundled UI is served from.
const ASSET_HOST: &str = "cleandark.assets";
const ASSET_ORIGIN: &str = "https://cleandark.assets/";

/// Persist download progress at most once per this many bytes received.
const DOWNLOAD_WRITE_INTERVAL_BYTES: i64 = 512 * 1024;

fn asset_url(page: &str) -> String {
    format!("{ASSET_ORIGIN}{page}")
}

/// Injected into every page before its own scripts run.
///
/// Deliberately tiny. It reads the declared theme colour — no screenshots, no
/// pixel sampling — and stops observing after a few seconds so a long-lived tab
/// never keeps a mutation observer running for nothing.
const BRIDGE_SCRIPT: &str = r#"
(function () {
  var post = function (msg) {
    try { window.chrome.webview.postMessage(msg); } catch (e) {}
  };
  var last = null;
  var report = function () {
    var meta = document.querySelector('meta[name="theme-color"]');
    var color = meta ? (meta.getAttribute('content') || '') : '';
    if (color !== last) { last = color; post({ cmd: 'themeColor', color: color }); }
  };
  var start = function () {
    report();
    if (!document.head) { return; }
    var observer = new MutationObserver(report);
    observer.observe(document.head, {
      childList: true, subtree: true, attributeFilter: ['content']
    });
    // Single-page apps set theme-color late; everything after this is noise.
    setTimeout(function () { observer.disconnect(); }, 8000);
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start, { once: true });
  } else {
    start();
  }
})();
"#;

/// WebView2 getters return their value through an out-parameter. This wraps
/// the pattern so a failed call reads as `false` instead of a panic.
fn read_bool<F>(get: F) -> bool
where
    F: FnOnce(*mut windows::core::BOOL) -> Result<()>,
{
    let mut value = windows::core::BOOL::default();
    get(&mut value).is_ok() && value.as_bool()
}

/// Same out-parameter pattern for the 64-bit counters on download operations.
fn read_i64<F>(get: F) -> i64
where
    F: FnOnce(*mut i64) -> Result<()>,
{
    let mut value = 0i64;
    if get(&mut value).is_ok() {
        value
    } else {
        0
    }
}

pub struct App {
    window: MainWindow,
    environment: Environment,
    chrome: ICoreWebView2,
    chrome_controller: ICoreWebView2Controller,

    tabs: RefCell<Vec<Tab>>,
    active: Cell<Option<u32>>,
    next_tab_id: Cell<u32>,

    blocker: RefCell<Blocker>,
    storage: Storage,
    vault: RefCell<Vault>,
    settings: RefCell<Settings>,
    paths: Paths,
    incognito: bool,

    /// Where the bundled UI lives. Resolved once at startup: `current_exe()` is
    /// a syscall, and this used to run on every tab creation.
    asset_dir: PathBuf,
    /// Extra height the chrome overlays over the page while a dropdown is open.
    overlay_height: Cell<i32>,
    /// Where a background filter refresh leaves its result.
    refresh_slot: std::sync::Arc<lists::RefreshSlot>,

    /// Blocked-request totals not yet written to the database.
    pending_counters: RefCell<PendingCounters>,
    /// Blocked requests since this window opened.
    session_blocked: Cell<u64>,
    /// Holds the previous CPU reading so a rate can be derived.
    sampler: RefCell<ProcessSampler>,

    /// Chrome updates marked but not yet sent. See [`crate::browser::pending`].
    ui_pending: RefCell<PendingUi>,
    /// A minimized window paints nothing, so even its foreground tab becomes
    /// reclaimable.
    minimized: Cell<bool>,

    self_ref: RefCell<Weak<App>>,
}

impl App {
    pub fn launch(incognito: bool, asset_dir: PathBuf) -> Result<Rc<Self>> {
        let paths = Paths::resolve().map_err(|e| {
            windows::core::Error::new(windows::core::HRESULT(-1), format!("profile dir: {e}"))
        })?;
        let settings = Settings::load(&paths.settings);

        let title = if incognito { "CleanDark \u{2014} Private" } else { "CleanDark" };
        let win = MainWindow::create(title, incognito)?;

        let environment = Environment::create(&paths.webview_data, &settings)?;

        // The chrome controller is created first so it stays above every
        // content host in z-order.
        let chrome_controller = environment.create_controller(win.chrome_host(), incognito)?;
        let chrome = unsafe { chrome_controller.CoreWebView2()? };
        wv::harden(&chrome, Role::Chrome, &settings)?;
        wv::harden_profile(&chrome)?;
        wv::map_asset_folder(&chrome, ASSET_HOST, &asset_dir)?;

        // Incognito never touches the disk; a normal window opens the profile
        // database and tidies up after whatever the last run left behind.
        let storage = if incognito {
            Storage::in_memory()
        } else {
            Storage::open(&paths.database)
        }
        .map_err(|e| windows::core::Error::new(windows::core::HRESULT(-1), format!("storage: {e}")))?;
        if !incognito {
            let _ = storage.interrupt_stale_downloads();
            let _ = storage.prune_downloads(settings.download_history_days);
        }

        let blocker = load_blocker(&paths, &settings);
        let vault = Vault::new(&paths.vault);

        let app = Rc::new(Self {
            window: win,
            environment,
            chrome,
            chrome_controller,
            tabs: RefCell::new(Vec::new()),
            active: Cell::new(None),
            next_tab_id: Cell::new(1),
            blocker: RefCell::new(blocker),
            storage,
            vault: RefCell::new(vault),
            settings: RefCell::new(settings),
            paths,
            incognito,
            asset_dir,
            overlay_height: Cell::new(0),
            refresh_slot: std::sync::Arc::new(lists::RefreshSlot::new()),
            pending_counters: RefCell::new(PendingCounters::default()),
            session_blocked: Cell::new(0),
            sampler: RefCell::new(ProcessSampler::new()),
            ui_pending: RefCell::new(PendingUi::default()),
            minimized: Cell::new(false),
            self_ref: RefCell::new(Weak::new()),
        });
        *app.self_ref.borrow_mut() = Rc::downgrade(&app);

        app.window.set_delegate(app.clone());
        app.wire_chrome_events();

        wv::navigate(&app.chrome, &asset_url("chrome.html"))?;

        app.layout();
        app.window.show();
        app.open_tab(&asset_url("newtab.html"), true)?;
        app.spawn_filter_refresh();

        Ok(app)
    }

    fn me(&self) -> Option<Rc<App>> {
        self.self_ref.borrow().upgrade()
    }

    // ---------------------------------------------------------------- tabs

    pub fn open_tab(self: &Rc<Self>, url: &str, activate: bool) -> Result<u32> {
        let id = self.next_tab_id.get();
        self.next_tab_id.set(id + 1);

        let host = self.window.create_content_host()?;
        self.tabs.borrow_mut().push(Tab::new(id, host, url.to_string()));

        self.attach_webview(id)?;
        if activate {
            self.activate_tab(id);
        } else {
            self.layout();
            self.mark_tabs();
            self.update_reclaim_timer();
        }
        Ok(id)
    }

    /// Create (or recreate) the WebView backing a tab and wire its events.
    fn attach_webview(self: &Rc<Self>, tab_id: u32) -> Result<()> {
        let (host_window, url) = {
            let tabs = self.tabs.borrow();
            let Some(tab) = tabs.iter().find(|t| t.id == tab_id) else {
                return Ok(());
            };
            if tab.is_live() {
                return Ok(());
            }
            (tab.host_window, tab.url.clone())
        };

        let controller = self.environment.create_controller(host_window, self.incognito)?;
        let webview = unsafe { controller.CoreWebView2()? };
        {
            let settings = self.settings.borrow();
            wv::harden(&webview, Role::Content, &settings)?;
        }
        wv::map_asset_folder(&webview, ASSET_HOST, &self.asset_dir)?;
        wv::add_startup_script(&webview, BRIDGE_SCRIPT)?;

        self.wire_tab_events(tab_id, &webview);
        self.register_resource_filters(&webview)?;

        {
            let mut tabs = self.tabs.borrow_mut();
            if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                tab.controller = Some(controller);
                tab.webview = Some(webview.clone());
                tab.power = TabPower::Normal;
            }
        }

        if !url.is_empty() {
            wv::navigate(&webview, &url)?;
        }
        Ok(())
    }

    pub fn activate_tab(self: &Rc<Self>, id: u32) {
        if self.active.get() == Some(id) {
            return;
        }
        self.active.set(Some(id));

        let needs_reload = {
            let mut tabs = self.tabs.borrow_mut();
            let mut needs_reload = false;
            for tab in tabs.iter_mut() {
                let is_active = tab.id == id;
                if is_active {
                    tab.last_active = Instant::now();
                    if tab.power == TabPower::Discarded {
                        needs_reload = true;
                    }
                }
                if let Some(controller) = &tab.controller {
                    unsafe {
                        let _ = controller.SetIsVisible(is_active);
                    }
                }
            }
            needs_reload
        };

        if needs_reload {
            let _ = self.attach_webview(id);
        } else {
            // Undo whatever the reclaim policy did while the tab was hidden.
            let webview = self.tab_webview(id);
            if let Some(webview) = webview {
                let _ = wv::resume(&webview);
                let _ = wv::set_low_memory(&webview, false);
            }
            let mut tabs = self.tabs.borrow_mut();
            if let Some(tab) = tabs.iter_mut().find(|t| t.id == id) {
                tab.power = TabPower::Normal;
            }
        }

        self.layout();
        self.mark_tabs();
        self.mark_navigation(id);
        self.update_reclaim_timer();
    }

    pub fn close_tab(self: &Rc<Self>, id: u32) {
        let (index, host) = {
            let tabs = self.tabs.borrow();
            let Some(index) = tabs.iter().position(|t| t.id == id) else {
                return;
            };
            (index, tabs[index].host_window)
        };

        {
            let mut tabs = self.tabs.borrow_mut();
            let mut tab = tabs.remove(index);
            tab.discard();
        }
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(host);
        }

        let remaining = self.tabs.borrow().len();
        if remaining == 0 {
            self.window.close();
            return;
        }

        if self.active.get() == Some(id) {
            let next = {
                let tabs = self.tabs.borrow();
                tabs[index.min(remaining - 1)].id
            };
            self.active.set(None);
            self.activate_tab(next);
        } else {
            self.layout();
            self.mark_tabs();
        }
        self.update_reclaim_timer();
    }

    fn tab_webview(&self, id: u32) -> Option<ICoreWebView2> {
        self.tabs.borrow().iter().find(|t| t.id == id).and_then(|t| t.webview.clone())
    }

    fn active_webview(&self) -> Option<ICoreWebView2> {
        self.active.get().and_then(|id| self.tab_webview(id))
    }

    // -------------------------------------------------------------- layout

    fn layout(&self) {
        let (width, height) = self.window.client_size();
        if width <= 0 || height <= 0 {
            return;
        }
        let chrome_h = self.window.chrome_height();
        let overlay = self.overlay_height.get().clamp(0, (height - chrome_h).max(0));

        // The chrome strip, plus any dropdown height. The content host keeps
        // its size, so an open dropdown occludes the page instead of
        // reflowing it.
        self.window.place_host(
            self.window.chrome_host(),
            RECT { left: 0, top: 0, right: width, bottom: chrome_h + overlay },
            true,
        );
        unsafe {
            let _ = self.chrome_controller.SetBounds(RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: chrome_h + overlay,
            });
            let _ = self.chrome_controller.SetIsVisible(true);
        }

        // Only the visible tab is positioned. `SetBounds` on a WebView forces a
        // renderer relayout even when the WebView is hidden, so resizing every
        // background tab would multiply the cost of a single window drag by the
        // tab count — with WM_SIZE arriving continuously while dragging. Hidden
        // tabs keep stale bounds and are laid out again by `activate_tab`.
        let active = self.active.get();
        let tabs = self.tabs.borrow();
        for tab in tabs.iter() {
            if Some(tab.id) != active {
                self.window.hide_host(tab.host_window);
                continue;
            }
            self.window.place_host(
                tab.host_window,
                RECT { left: 0, top: chrome_h, right: width, bottom: height },
                true,
            );
            if let Some(controller) = &tab.controller {
                unsafe {
                    let _ = controller.SetBounds(RECT {
                        left: 0,
                        top: 0,
                        right: width,
                        bottom: (height - chrome_h).max(0),
                    });
                    let _ = controller.SetIsVisible(true);
                }
            }
        }
    }

    // ------------------------------------------------------------- pushing

    fn post(&self, event: &Event<'_>) {
        let _ = wv::post_json(&self.chrome, &event.to_json());
    }

    /// Mark chrome state as stale and make sure exactly one flush is queued.
    ///
    /// The message is posted only on the empty -> non-empty transition, so a
    /// burst of twenty events during a page load queues one message, not twenty.
    fn mark<F: FnOnce(&mut PendingUi)>(&self, mark: F) {
        let should_post = {
            let mut pending = self.ui_pending.borrow_mut();
            let was_empty = pending.is_empty();
            mark(&mut pending);
            was_empty && !pending.is_empty()
        };
        if should_post {
            MainWindow::post_app_message(self.window.hwnd(), WM_APP_FLUSH_UI);
        }
    }

    fn mark_tabs(&self) {
        self.mark(PendingUi::mark_tabs);
    }

    /// Only the active tab's toolbar state is ever rendered, so marking a
    /// background tab would be work thrown away at flush time.
    fn mark_navigation(&self, tab_id: u32) {
        if self.active.get() == Some(tab_id) {
            self.mark(|pending| pending.mark_navigation(tab_id));
        }
    }

    fn mark_downloads(&self) {
        self.mark(PendingUi::mark_downloads);
    }

    /// Send everything that was marked since the last flush.
    fn flush_ui(&self) {
        let pending = self.ui_pending.borrow_mut().take();
        if pending.tabs {
            self.push_tabs();
        }
        if let Some(tab_id) = pending.navigation {
            self.push_navigation(tab_id);
        }
        if pending.downloads {
            self.push_downloads();
        }
    }

    fn push_tabs(&self) {
        let views: Vec<_> = self.tabs.borrow().iter().map(|t| t.to_view()).collect();
        self.post(&Event::Tabs { tabs: views, active: self.active.get() });
    }

    fn push_navigation(&self, tab_id: u32) {
        let tabs = self.tabs.borrow();
        let Some(tab) = tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        // Both of these used to be recomputed here — a SQL query and a URL
        // parse — on every navigation event, of which there are several per
        // page load. They now live on the tab and change only when the URL or
        // the bookmark does.
        let shield_active = {
            let blocker = self.blocker.borrow();
            blocker.is_enabled() && !blocker.is_host_allowed(&tab.host)
        };

        self.post(&Event::Navigation {
            id: tab.id,
            url: &tab.url,
            title: &tab.title,
            can_go_back: tab.can_go_back,
            can_go_forward: tab.can_go_forward,
            loading: tab.loading,
            bookmarked: tab.bookmarked,
            secure: tab.url.starts_with("https://"),
            blocked: tab.blocked,
            shield_active,
        });

        if self.settings.borrow().chameleon_enabled {
            self.post(&Event::Accent { color: tab.theme_color.clone() });
        }
    }

    fn push_window_mode(&self) {
        self.post(&Event::WindowMode {
            incognito: self.incognito,
            maximized: self.window.is_maximized(),
        });
    }

    fn toast(&self, message: impl Into<String>, kind: ToastKind) {
        self.post(&Event::Toast { message: message.into(), kind });
    }

    fn push_settings(&self) {
        let settings = self.settings.borrow();
        self.post(&Event::Settings {
            settings: SettingsView {
                chameleon_enabled: settings.chameleon_enabled,
                reduce_motion: settings.reduce_motion,
                accent: settings.accent.clone(),
                adblock_enabled: settings.adblock_enabled,
                search_template: settings.search_template.clone(),
                reclaim_mode: match settings.reclaim_mode {
                    ReclaimMode::Off => "off",
                    ReclaimMode::Balanced => "balanced",
                    ReclaimMode::Aggressive => "aggressive",
                },
            },
        });
    }

    fn save_settings(&self) {
        if self.incognito {
            return;
        }
        let _ = self.settings.borrow().save(&self.paths.settings);
    }

    // ---------------------------------------------------------- blocking

    fn register_resource_filters(&self, webview: &ICoreWebView2) -> Result<()> {
        let all = windows::core::HSTRING::from("*");
        for kind in ResourceKind::intercepted() {
            unsafe {
                webview.AddWebResourceRequestedFilter(&all, kind.to_webview2())?;
            }
        }
        Ok(())
    }

    fn spawn_filter_refresh(self: &Rc<Self>) {
        let settings = self.settings.borrow();
        if !settings.adblock_enabled {
            return;
        }
        if lists::cache_is_fresh(&self.paths.filter_cache, settings.filter_refresh_days) {
            return;
        }

        let request = lists::RefreshRequest {
            urls: settings.filter_list_urls.clone(),
            lists_dir: self.paths.filter_lists.clone(),
            cache_path: self.paths.filter_cache.clone(),
        };
        drop(settings);

        let slot = self.refresh_slot.clone();
        // HWND is not Send; the numeric handle is, and the window outlives the
        // thread because closing it drains the message queue first.
        let hwnd_bits = self.window.hwnd().0 as isize;

        std::thread::spawn(move || {
            if let Some(compiled) = lists::refresh(&request) {
                slot.put(compiled);
                MainWindow::post_app_message(
                    HWND(hwnd_bits as *mut std::ffi::c_void),
                    WM_APP_FILTERS_READY,
                );
            }
        });
    }

    // ------------------------------------------------------------- events

    fn wire_chrome_events(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let mut token = 0;
        unsafe {
            let _ = self.chrome.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
                    let Some(app) = weak.upgrade() else { return Ok(()) };
                    let Some(args) = args else { return Ok(()) };

                    let mut json = PWSTR::null();
                    if args.WebMessageAsJson(&mut json).is_ok() {
                        let json = take_pwstr(json);
                        if let Some(command) = Command::parse(&json) {
                            app.handle_command(command, true);
                        }
                    }
                    Ok(())
                })),
                &mut token,
            );
        }
    }

    fn wire_tab_events(self: &Rc<Self>, tab_id: u32, webview: &ICoreWebView2) {
        let mut token = 0;

        // --- navigation starting: main-frame bookkeeping and blocking ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                let _ = webview.add_NavigationStarting(
                    &NavigationStartingEventHandler::create(Box::new(move |_sender, args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let Some(args) = args else { return Ok(()) };

                        let mut uri = PWSTR::null();
                        if args.Uri(&mut uri).is_err() {
                            return Ok(());
                        }
                        let uri = take_pwstr(uri);
                        let is_redirect = read_bool(|out| args.IsRedirected(out));

                        if app.should_block_main_frame(&uri) {
                            let _ = args.SetCancel(true);
                            app.toast(
                                format!("Blocked navigation to {}", util::host_of(&uri).unwrap_or(uri)),
                                ToastKind::Warning,
                            );
                            return Ok(());
                        }

                        {
                            let mut tabs = app.tabs.borrow_mut();
                            if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                                tab.pending_main_frame = Some(uri);
                                tab.loading = true;
                                if !is_redirect {
                                    tab.blocked = 0;
                                    tab.theme_color = None;
                                }
                            }
                        }
                        app.mark_tabs();
                        app.mark_navigation(tab_id);
                        Ok(())
                    })),
                    &mut token,
                );
            }
        }

        // --- source changed: the address bar follows the committed URL ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                let _ = webview.add_SourceChanged(
                    &SourceChangedEventHandler::create(Box::new(move |sender, _args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let Some(sender) = sender else { return Ok(()) };

                        let mut source = PWSTR::null();
                        if sender.Source(&mut source).is_ok() {
                            let source = take_pwstr(source);
                            let bookmarked =
                                app.storage.is_bookmarked(&source).unwrap_or(false);
                            let mut tabs = app.tabs.borrow_mut();
                            if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                                tab.set_url(source);
                                tab.bookmarked = bookmarked;
                            }
                        }
                        app.mark_navigation(tab_id);
                        Ok(())
                    })),
                    &mut token,
                );
            }
        }

        // --- history changed: back/forward availability ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                let _ = webview.add_HistoryChanged(
                    &HistoryChangedEventHandler::create(Box::new(move |sender, _args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let Some(sender) = sender else { return Ok(()) };
                        let back = read_bool(|out| sender.CanGoBack(out));
                        let forward = read_bool(|out| sender.CanGoForward(out));
                        {
                            let mut tabs = app.tabs.borrow_mut();
                            if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                                tab.can_go_back = back;
                                tab.can_go_forward = forward;
                            }
                        }
                        app.mark_navigation(tab_id);
                        Ok(())
                    })),
                    &mut token,
                );
            }
        }

        // --- navigation completed: record history ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                let _ = webview.add_NavigationCompleted(
                    &NavigationCompletedEventHandler::create(Box::new(move |_sender, _args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let (url, title) = {
                            let mut tabs = app.tabs.borrow_mut();
                            let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) else {
                                return Ok(());
                            };
                            tab.loading = false;
                            tab.pending_main_frame = None;
                            (tab.url.clone(), tab.title.clone())
                        };

                        if app.settings.borrow().save_history {
                            let _ = app.storage.record_visit(&url, &title);
                        }
                        app.mark_tabs();
                        app.mark_navigation(tab_id);
                        Ok(())
                    })),
                    &mut token,
                );
            }
        }

        // --- title ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                let _ = webview.add_DocumentTitleChanged(
                    &DocumentTitleChangedEventHandler::create(Box::new(move |sender, _args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let Some(sender) = sender else { return Ok(()) };
                        let mut title = PWSTR::null();
                        if sender.DocumentTitle(&mut title).is_ok() {
                            let title = take_pwstr(title);
                            let url = {
                                let mut tabs = app.tabs.borrow_mut();
                                match tabs.iter_mut().find(|t| t.id == tab_id) {
                                    Some(tab) => {
                                        tab.title = title.clone();
                                        tab.url.clone()
                                    }
                                    None => return Ok(()),
                                }
                            };
                            if app.settings.borrow().save_history {
                                let _ = app.storage.update_title(&url, &title);
                            }
                        }
                        app.mark_tabs();
                        Ok(())
                    })),
                    &mut token,
                );
            }
        }

        // --- audio, so the reclaim policy never freezes a playing tab ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                if let Ok(webview8) = webview.cast::<ICoreWebView2_8>() {
                    let _ = webview8.add_IsDocumentPlayingAudioChanged(
                        &IsDocumentPlayingAudioChangedEventHandler::create(Box::new(
                            move |sender, _args| {
                                let Some(app) = weak.upgrade() else { return Ok(()) };
                                let audible = sender
                                    .as_ref()
                                    .and_then(|s| s.cast::<ICoreWebView2_8>().ok())
                                    .map(|w| read_bool(|out| w.IsDocumentPlayingAudio(out)))
                                    .unwrap_or(false);
                                {
                                    let mut tabs = app.tabs.borrow_mut();
                                    if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                                        tab.audible = audible;
                                    }
                                }
                                app.mark_tabs();
                                Ok(())
                            },
                        )),
                        &mut token,
                    );
                }
            }
        }

        // --- new window: keep everything in one window as a tab ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                let _ = webview.add_NewWindowRequested(
                    &NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let Some(args) = args else { return Ok(()) };
                        let mut uri = PWSTR::null();
                        if args.Uri(&mut uri).is_ok() {
                            let uri = take_pwstr(uri);
                            let _ = args.SetHandled(true);
                            let _ = app.open_tab(&uri, true);
                        }
                        Ok(())
                    })),
                    &mut token,
                );
            }
        }

        // --- downloads ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                if let Ok(webview4) = webview.cast::<ICoreWebView2_4>() {
                    let _ = webview4.add_DownloadStarting(
                        &DownloadStartingEventHandler::create(Box::new(move |_sender, args| {
                            let Some(app) = weak.upgrade() else { return Ok(()) };
                            let Some(args) = args else { return Ok(()) };
                            app.on_download_starting(&args);
                            Ok(())
                        })),
                        &mut token,
                    );
                }
            }
        }

        // --- page messages: theme colour, and commands from our own pages ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                let _ = webview.add_WebMessageReceived(
                    &WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let Some(args) = args else { return Ok(()) };

                        let mut source = PWSTR::null();
                        let source = if args.Source(&mut source).is_ok() {
                            take_pwstr(source)
                        } else {
                            String::new()
                        };

                        let mut json = PWSTR::null();
                        if args.WebMessageAsJson(&mut json).is_err() {
                            return Ok(());
                        }
                        let json = take_pwstr(json);
                        let Some(command) = Command::parse(&json) else { return Ok(()) };

                        // Any page can post a message. Only our own bundled
                        // pages are trusted with the full command surface;
                        // everything else may report a theme colour and
                        // nothing more.
                        let trusted = source.starts_with(ASSET_ORIGIN);
                        match (&command, trusted) {
                            (Command::ThemeColor { color }, _) => {
                                app.on_theme_color(tab_id, color);
                            }
                            (_, true) => app.handle_command(command, true),
                            (_, false) => {}
                        }
                        Ok(())
                    })),
                    &mut token,
                );
            }
        }

        // --- the blocker: the hot path ---
        {
            let weak = Rc::downgrade(self);
            unsafe {
                let _ = webview.add_WebResourceRequested(
                    &WebResourceRequestedEventHandler::create(Box::new(move |_sender, args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let Some(args) = args else { return Ok(()) };
                        app.on_web_resource_requested(tab_id, &args);
                        Ok(())
                    })),
                    &mut token,
                );
            }
        }
    }

    fn on_web_resource_requested(
        &self,
        tab_id: u32,
        args: &ICoreWebView2WebResourceRequestedEventArgs,
    ) {
        if !self.blocker.borrow().is_enabled() {
            return;
        }

        let (uri, kind) = unsafe {
            let Ok(request) = args.Request() else { return };
            let mut uri = PWSTR::null();
            if request.Uri(&mut uri).is_err() {
                return;
            }
            let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT::default();
            if args.ResourceContext(&mut context).is_err() {
                return;
            }
            (take_pwstr(uri), ResourceKind::from_webview2(context))
        };

        // The decision is taken under a single shared borrow of the tab list,
        // reading `url`, `host` and `pending_main_frame` in place. Cloning them
        // would put two heap allocations on every intercepted request.
        // `tabs` and `blocker` are separate cells, so holding both is fine.
        let filter_type = {
            let tabs = self.tabs.borrow();
            let Some(tab) = tabs.iter().find(|t| t.id == tab_id) else {
                return;
            };
            let Some(filter_type) =
                filter_type_for(kind, &uri, tab.pending_main_frame.as_deref())
            else {
                return;
            };
            let blocked = self.blocker.borrow_mut().should_block(
                &uri,
                &tab.url,
                &tab.host,
                filter_type,
            );
            if !blocked {
                return;
            }
            filter_type
        };

        if let Ok(response) = self.environment.blocked_response() {
            unsafe {
                let _ = args.SetResponse(&response);
            }
        }

        // Lifetime counters accumulate in memory; writing a database row per
        // blocked request would put a write on the page-load hot path.
        self.session_blocked.set(self.session_blocked.get().saturating_add(1));
        {
            let mut pending = self.pending_counters.borrow_mut();
            pending.record(filter_type);
            if pending.should_flush() {
                let (requests, bytes) = pending.take();
                drop(pending);
                self.write_counters(requests, bytes);
            }
        }

        let count = {
            let mut tabs = self.tabs.borrow_mut();
            match tabs.iter_mut().find(|t| t.id == tab_id) {
                Some(tab) => {
                    tab.blocked += 1;
                    tab.blocked
                }
                None => return,
            }
        };
        // The badge only needs to be roughly right. A full `Navigation` event
        // would serialize the URL and title and re-read the bookmark state; this
        // one carries two integers, and is sent once per 25 blocked requests.
        if count % 25 == 1 {
            self.post(&Event::Blocked { id: tab_id, count });
        }
    }

    /// Main-frame navigations are decided here rather than in the resource
    /// interceptor so a block leaves the user on their current page instead of
    /// a blank tab.
    fn should_block_main_frame(&self, url: &str) -> bool {
        if url.starts_with(ASSET_ORIGIN) || url.starts_with("about:") {
            return false;
        }
        let mut blocker = self.blocker.borrow_mut();
        if !blocker.is_enabled() {
            return false;
        }
        // A top-level navigation is its own source, so the host of the target
        // is also the host the allowlist is checked against.
        let host = util::host_of(url).unwrap_or_default();
        blocker.should_block(url, url, &host, "document")
    }

    fn on_theme_color(&self, tab_id: u32, color: &str) {
        let normalised = normalise_theme_color(color);
        {
            let mut tabs = self.tabs.borrow_mut();
            let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) else {
                return;
            };
            tab.theme_color = normalised.clone();
        }
        if self.active.get() == Some(tab_id) && self.settings.borrow().chameleon_enabled {
            self.post(&Event::Accent { color: normalised });
        }
    }

    fn on_download_starting(&self, args: &ICoreWebView2DownloadStartingEventArgs) {
        unsafe {
            let Ok(operation) = args.DownloadOperation() else { return };

            let mut uri = PWSTR::null();
            let url = if operation.Uri(&mut uri).is_ok() { take_pwstr(uri) } else { String::new() };
            let mut path = PWSTR::null();
            let target = if operation.ResultFilePath(&mut path).is_ok() {
                take_pwstr(path)
            } else {
                String::new()
            };
            let total = read_i64(|out| operation.TotalBytesToReceive(out));

            let Ok(id) = self.storage.start_download(&url, &target, total) else {
                return;
            };

            // We show progress in our own downloads page, so WebView2's default
            // download popup would be a second, redundant UI.
            let _ = args.SetHandled(true);

            let mut token = 0;
            if let Some(app) = self.me() {
                // Both handlers read the operation from the `sender` argument
                // rather than capturing a clone of it. Capturing would create a
                // COM reference cycle — the operation owns the handler, the
                // handler owns the operation — and neither would ever be
                // released, leaking one download operation per file.
                let weak = Rc::downgrade(&app);
                // Persisted progress is throttled: BytesReceivedChanged fires
                // many times a second on a fast link, and a WAL write per event
                // is pure overhead when the row is only read by the downloads
                // page. The UI event itself is cheap and stays unthrottled so
                // the progress bar remains smooth.
                let mut written_at = 0i64;
                let _ = operation.add_BytesReceivedChanged(
                    &webview2_com::BytesReceivedChangedEventHandler::create(Box::new(
                        move |sender, _args| {
                            let Some(app) = weak.upgrade() else { return Ok(()) };
                            let Some(op) = sender else { return Ok(()) };

                            let received = read_i64(|out| op.BytesReceived(out));
                            if received - written_at >= DOWNLOAD_WRITE_INTERVAL_BYTES {
                                written_at = received;
                                let _ = app.storage.update_download_progress(id, received);
                            }
                            app.post(&Event::DownloadProgress {
                                id,
                                received,
                                total: read_i64(|out| op.TotalBytesToReceive(out)),
                            });
                            Ok(())
                        },
                    )),
                    &mut token,
                );

                let weak = Rc::downgrade(&app);
                let _ = operation.add_StateChanged(
                    &StateChangedEventHandler::create(Box::new(move |sender, _args| {
                        let Some(app) = weak.upgrade() else { return Ok(()) };
                        let Some(op) = sender else { return Ok(()) };

                        let mut raw = COREWEBVIEW2_DOWNLOAD_STATE::default();
                        if op.State(&mut raw).is_err() {
                            return Ok(());
                        }
                        let state = match raw {
                            COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED => DownloadState::Completed,
                            COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED => DownloadState::Interrupted,
                            // Still in progress: the byte counter already
                            // covers it, so there is nothing to finalise.
                            _ => return Ok(()),
                        };
                        // The final size is written here regardless of the
                        // throttle above, so a finished row is always exact.
                        let _ = app.storage.finish_download_with_size(
                            id,
                            state,
                            read_i64(|out| op.TotalBytesToReceive(out)),
                            read_i64(|out| op.BytesReceived(out)),
                        );
                        app.mark_downloads();
                        Ok(())
                    })),
                    &mut token,
                );
            }

            self.mark_downloads();
            self.toast("Download started", ToastKind::Info);
        }
    }

    /// Persist accumulated counters. Incognito windows never write.
    fn write_counters(&self, requests: u64, bytes: u64) {
        if self.incognito || requests == 0 {
            return;
        }
        let _ = self.storage.add_counter(counters::BLOCKED_TOTAL, requests);
        let _ = self.storage.add_counter(counters::BYTES_SAVED_TOTAL, bytes);
    }

    fn flush_counters(&self) {
        // Checked before taking a mutable borrow: this runs on every reclaim
        // tick, and the common case is that nothing has accumulated.
        if self.pending_counters.borrow().is_empty() {
            return;
        }
        let (requests, bytes) = self.pending_counters.borrow_mut().take();
        self.write_counters(requests, bytes);
    }

    /// Gather everything the privacy hub shows.
    ///
    /// Called from the new-tab page's poll, so the poll interval is also the
    /// CPU averaging window. Nothing here runs when no new-tab page is open.
    fn push_privacy_stats(&self) {
        self.flush_counters();

        let sample = self.sampler.borrow_mut().sample(&self.environment.process_ids());

        // An incognito window has no persisted history, so its hub shows the
        // session only rather than leaking the main profile's totals.
        let (blocked_total, bytes_saved) = if self.incognito {
            (self.session_blocked.get(), 0)
        } else {
            (
                self.storage.counter(counters::BLOCKED_TOTAL).unwrap_or(0),
                self.storage.counter(counters::BYTES_SAVED_TOTAL).unwrap_or(0),
            )
        };

        self.post(&Event::Privacy {
            stats: PrivacyStats {
                blocked_total,
                blocked_session: self.session_blocked.get(),
                bytes_saved,
                time_saved_ms: stats::estimated_time_saved_ms(blocked_total, bytes_saved),
                memory_bytes: sample.memory_bytes,
                cpu_percent: sample.cpu_percent,
                process_count: sample.process_count,
                blocking_enabled: self.blocker.borrow().is_enabled(),
            },
        });
    }

    fn push_downloads(&self) {
        if let Ok(items) = self.storage.recent_downloads(200) {
            self.post(&Event::Downloads { items });
        }
    }

    // ------------------------------------------------------------ commands

    pub fn handle_command(self: &Rc<Self>, command: Command, trusted: bool) {
        if !trusted {
            return;
        }
        match command {
            // ---- navigation ----
            Command::OmniboxSubmit { text, new_tab } => {
                let url = {
                    let settings = self.settings.borrow();
                    util::omnibox_to_url(&text, &settings.search_template)
                };
                if url.is_empty() {
                    return;
                }
                if new_tab {
                    let _ = self.open_tab(&url, true);
                } else if let Some(webview) = self.active_webview() {
                    let _ = wv::navigate(&webview, &url);
                } else {
                    let _ = self.open_tab(&url, true);
                }
            }
            Command::Navigate { url } => {
                if let Some(webview) = self.active_webview() {
                    let _ = wv::navigate(&webview, &url);
                }
            }
            Command::Back => {
                if let Some(webview) = self.active_webview() {
                    unsafe {
                        let _ = webview.GoBack();
                    }
                }
            }
            Command::Forward => {
                if let Some(webview) = self.active_webview() {
                    unsafe {
                        let _ = webview.GoForward();
                    }
                }
            }
            Command::Reload { bypass_cache } => {
                if let Some(webview) = self.active_webview() {
                    unsafe {
                        if bypass_cache {
                            // Reload() honours the cache; a hard reload is a
                            // navigation with cache-control disabled, which the
                            // DevTools protocol exposes directly.
                            let _ = webview.CallDevToolsProtocolMethod(
                                &windows::core::HSTRING::from("Page.reload"),
                                &windows::core::HSTRING::from(r#"{"ignoreCache":true}"#),
                                None,
                            );
                        } else {
                            let _ = webview.Reload();
                        }
                    }
                }
            }
            Command::Stop => {
                if let Some(webview) = self.active_webview() {
                    unsafe {
                        let _ = webview.Stop();
                    }
                }
            }

            // ---- tabs ----
            Command::NewTab { url } => {
                let target = url.unwrap_or_else(|| asset_url("newtab.html"));
                let _ = self.open_tab(&target, true);
            }
            Command::CloseTab { id } => self.close_tab(id),
            Command::ActivateTab { id } => self.activate_tab(id),
            Command::MoveTab { id, to_index } => {
                let mut tabs = self.tabs.borrow_mut();
                if let Some(from) = tabs.iter().position(|t| t.id == id) {
                    let to = to_index.min(tabs.len().saturating_sub(1));
                    let tab = tabs.remove(from);
                    tabs.insert(to, tab);
                }
                drop(tabs);
                self.mark_tabs();
            }

            // ---- window ----
            Command::WindowMinimize => self.window.minimize(),
            Command::WindowToggleMaximize => self.window.toggle_maximize(),
            Command::WindowClose => self.window.close(),
            Command::NewIncognitoWindow => {
                // A private window is a fresh process: it keeps the in-memory
                // profile genuinely separate and means closing it frees
                // everything at once.
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("--incognito").spawn();
                }
            }
            Command::ChromeOverlayHeight { px } => {
                let dpi = self.window.dpi();
                self.overlay_height.set(window::scale(px as i32, dpi));
                self.layout();
            }

            Command::ThemeColor { color } => {
                if let Some(id) = self.active.get() {
                    self.on_theme_color(id, &color);
                }
            }

            // ---- history ----
            Command::QueryHistory { query, limit } => {
                if let Ok(items) = self.storage.search_history(&query, limit.min(1000)) {
                    self.post(&Event::History { items });
                }
            }
            Command::DeleteHistoryEntry { id } => {
                let _ = self.storage.delete_visit(id);
                self.handle_command(
                    Command::QueryHistory { query: String::new(), limit: 200 },
                    true,
                );
            }
            Command::ClearHistory { since_millis } => {
                let _ = match since_millis {
                    Some(millis) => self.storage.clear_history_since(millis),
                    None => self.storage.clear_history(),
                };
                self.toast("History cleared", ToastKind::Info);
                self.handle_command(
                    Command::QueryHistory { query: String::new(), limit: 200 },
                    true,
                );
            }

            // ---- downloads ----
            Command::QueryDownloads => self.push_downloads(),
            Command::RemoveDownload { id } => {
                let _ = self.storage.remove_download(id);
                self.push_downloads();
            }
            Command::ClearDownloads => {
                // Only the list is cleared; files already on disk stay.
                let _ = self.storage.clear_downloads();
                self.push_downloads();
            }
            Command::CancelDownload { id } => {
                let _ = self.storage.finish_download(id, DownloadState::Cancelled);
                self.push_downloads();
            }
            Command::OpenDownload { id } => self.open_download(id, false),
            Command::ShowDownloadInFolder { id } => self.open_download(id, true),

            // ---- bookmarks ----
            Command::QueryBookmarks => {
                if let Ok(items) = self.storage.bookmarks() {
                    self.post(&Event::Bookmarks { items });
                }
            }
            Command::ToggleBookmark => {
                let Some(id) = self.active.get() else { return };
                let (url, title) = {
                    let tabs = self.tabs.borrow();
                    match tabs.iter().find(|t| t.id == id) {
                        // `display_title` borrows from the tab, so it has to be
                        // owned before the guard is dropped.
                        Some(tab) => (tab.url.clone(), tab.display_title().into_owned()),
                        None => return,
                    }
                };
                match self.storage.toggle_bookmark(&url, &title) {
                    Ok(state) => {
                        // Keep the cached flag in step with the database so the
                        // toolbar never has to query it again.
                        let mut tabs = self.tabs.borrow_mut();
                        if let Some(tab) = tabs.iter_mut().find(|t| t.id == id) {
                            tab.bookmarked = state;
                        }
                        drop(tabs);
                        self.toast(
                            if state { "Bookmarked" } else { "Bookmark removed" },
                            ToastKind::Info,
                        );
                    }
                    Err(_) => self.toast("Could not save bookmark", ToastKind::Error),
                }
                self.push_navigation(id);
            }
            Command::RemoveBookmark { id } => {
                let _ = self.storage.remove_bookmark(id);
                self.handle_command(Command::QueryBookmarks, true);
            }
            Command::RenameBookmark { id, title } => {
                let _ = self.storage.rename_bookmark(id, &title);
                self.handle_command(Command::QueryBookmarks, true);
            }
            Command::ReorderBookmarks { ids } => {
                let _ = self.storage.reorder_bookmarks(&ids);
                self.handle_command(Command::QueryBookmarks, true);
            }

            // ---- speed dials ----
            Command::QuerySpeedDials => self.push_speed_dials(),
            Command::AddSpeedDial { url, title, accent } => {
                if self.storage.speed_dials_full().unwrap_or(false) {
                    self.toast("Speed dial grid is full", ToastKind::Warning);
                    return;
                }
                let _ = self.storage.add_speed_dial(&url, &title, &accent);
                self.push_speed_dials();
            }
            Command::RemoveSpeedDial { id } => {
                let _ = self.storage.remove_speed_dial(id);
                self.push_speed_dials();
            }
            Command::UpdateSpeedDial { id, title, accent } => {
                let _ = self.storage.update_speed_dial(id, &title, &accent);
                self.push_speed_dials();
            }
            Command::ReorderSpeedDials { ids } => {
                let _ = self.storage.reorder_speed_dials(&ids);
                self.push_speed_dials();
            }

            // ---- vault ----
            Command::VaultStatus => self.push_vault_state(None),
            Command::VaultCreate { master_password } => {
                if self.vault.borrow().exists() {
                    self.toast("A vault already exists", ToastKind::Error);
                    return;
                }
                match self.vault.borrow_mut().create(&master_password) {
                    Ok(()) => self.toast("Vault created", ToastKind::Info),
                    Err(e) => self.toast(format!("Could not create vault: {e}"), ToastKind::Error),
                }
                self.push_vault_state(None);
            }
            Command::VaultUnlock { master_password } => {
                let result = self.vault.borrow_mut().unlock(&master_password);
                match result {
                    Ok(()) => self.push_vault_state(None),
                    Err(VaultError::BadPassword) => {
                        self.toast("Wrong master password", ToastKind::Error);
                        self.push_vault_state(None);
                    }
                    Err(e) => {
                        self.toast(format!("Vault error: {e}"), ToastKind::Error);
                        self.push_vault_state(None);
                    }
                }
            }
            Command::VaultLock => {
                self.vault.borrow_mut().lock();
                self.push_vault_state(None);
            }
            Command::VaultList => {
                let items = self.vault.borrow().list().ok();
                self.push_vault_state(items);
            }
            Command::VaultReveal { id } => {
                if let Ok(Some(password)) = self.vault.borrow().reveal(id) {
                    self.post(&Event::VaultSecret { id, password });
                }
            }
            Command::VaultUpsert { host, username, password, note } => {
                let result = self.vault.borrow_mut().upsert(&host, &username, &password, &note);
                match result {
                    Ok(_) => self.toast("Credential saved", ToastKind::Info),
                    Err(e) => self.toast(format!("Could not save: {e}"), ToastKind::Error),
                }
                let items = self.vault.borrow().list().ok();
                self.push_vault_state(items);
            }
            Command::VaultRemove { id } => {
                let _ = self.vault.borrow_mut().remove(id);
                let items = self.vault.borrow().list().ok();
                self.push_vault_state(items);
            }
            Command::VaultChangeMasterPassword { master_password } => {
                let result = self.vault.borrow_mut().change_master_password(&master_password);
                match result {
                    Ok(()) => self.toast("Master password changed", ToastKind::Info),
                    Err(e) => self.toast(format!("Could not change: {e}"), ToastKind::Error),
                }
            }

            // ---- blocking ----
            Command::SetShieldForHost { host, blocking } => {
                {
                    let mut blocker = self.blocker.borrow_mut();
                    if blocking {
                        blocker.remove_host_exception(&host);
                    } else {
                        blocker.allow_host(&host);
                    }
                }
                if let Some(webview) = self.active_webview() {
                    unsafe {
                        let _ = webview.Reload();
                    }
                }
            }
            Command::SetAdblockEnabled { enabled } => {
                self.blocker.borrow_mut().set_enabled(enabled);
                self.settings.borrow_mut().adblock_enabled = enabled;
                self.save_settings();
                let stats = self.blocker.borrow().stats();
                self.post(&Event::BlockerStats { stats, enabled });
            }

            // ---- privacy hub ----
            Command::QueryPrivacyStats => self.push_privacy_stats(),
            Command::ResetPrivacyStats => {
                self.pending_counters.borrow_mut().take();
                self.session_blocked.set(0);
                let _ = self.storage.reset_counters();
                self.toast("Privacy statistics reset", ToastKind::Info);
                self.push_privacy_stats();
            }

            // ---- settings ----
            Command::QuerySettings => self.push_settings(),
            Command::SetChameleon { enabled } => {
                self.settings.borrow_mut().chameleon_enabled = enabled;
                self.save_settings();
                self.push_settings();
                if let Some(id) = self.active.get() {
                    self.push_navigation(id);
                }
                if !enabled {
                    self.post(&Event::Accent { color: None });
                }
            }
            Command::SetReduceMotion { enabled } => {
                self.settings.borrow_mut().reduce_motion = enabled;
                self.save_settings();
                self.push_settings();
            }
            Command::SetSearchTemplate { template } => {
                if template.contains("{q}") {
                    self.settings.borrow_mut().search_template = template;
                    self.save_settings();
                    self.push_settings();
                } else {
                    self.toast("Search URL must contain {q}", ToastKind::Error);
                }
            }
        }
    }

    fn push_speed_dials(&self) {
        match self.storage.speed_dials() {
            Ok(items) if !items.is_empty() => {
                self.post(&Event::SpeedDials { items, suggested: false })
            }
            _ => {
                // An empty grid falls back to the most-visited hosts so the new
                // tab page is useful before the user has curated anything.
                let items = self.storage.suggested_speed_dials(8).unwrap_or_default();
                self.post(&Event::SpeedDials { items, suggested: true });
            }
        }
    }

    fn push_vault_state(&self, items: Option<Vec<crate::vault::CredentialSummary>>) {
        let vault = self.vault.borrow();
        let state = if !vault.exists() {
            VaultState::Uninitialised
        } else if vault.is_unlocked() {
            VaultState::Unlocked
        } else {
            VaultState::Locked
        };
        drop(vault);
        self.post(&Event::Vault { state, items });
    }

    fn open_download(&self, id: i64, reveal: bool) {
        let Ok(Some(download)) = self.storage.get_download(id) else {
            return;
        };
        let path = std::path::PathBuf::from(&download.target_path);
        let result = if reveal {
            crate::platform::shell::reveal_in_explorer(&path)
        } else {
            crate::platform::shell::open_path(&path)
        };
        if result.is_err() {
            self.toast("File is no longer available", ToastKind::Warning);
        }
    }

    /// Run the reclaim timer only when it has something to do.
    ///
    /// With a single visible tab nothing is ever reclaimable — the foreground
    /// tab is exempt — so the timer would be a periodic wakeup that decides
    /// nothing. A browser that costs nothing while idle has to stop its own
    /// clocks, not just the page's.
    fn update_reclaim_timer(&self) {
        let needed = self.minimized.get() || self.tabs.borrow().len() > 1;
        let interval = if needed {
            reclaim::tick_interval_ms(&self.settings.borrow())
        } else {
            0
        };
        self.window.set_reclaim_timer(interval);
    }

    // ------------------------------------------------------------- reclaim

    fn reclaim(self: &Rc<Self>) {
        let active = self.active.get();
        let minimized = self.minimized.get();

        // Borrowed, not cloned: `Settings` owns several `String`s and a `Vec`
        // of filter-list URLs, and this runs on a timer. Nothing in the loop
        // touches the settings cell, so the borrow is safe to hold.
        let decisions: Vec<(u32, ReclaimAction)> = {
            let settings = self.settings.borrow();
            let tabs = self.tabs.borrow();
            tabs.iter()
                .map(|tab| {
                    let state = TabState {
                        idle_secs: tab.idle_secs(),
                        is_active: Some(tab.id) == active,
                        is_audible: tab.audible,
                        power: tab.power,
                    };
                    (tab.id, reclaim::decide(state, minimized, &settings))
                })
                .collect()
        };

        for (tab_id, action) in decisions {
            match action {
                ReclaimAction::None | ReclaimAction::Restore => {}
                ReclaimAction::LowerMemoryTarget => {
                    if let Some(webview) = self.tab_webview(tab_id) {
                        let _ = wv::set_low_memory(&webview, true);
                        let mut tabs = self.tabs.borrow_mut();
                        if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                            tab.power = TabPower::LowMemory;
                        }
                    }
                }
                ReclaimAction::Suspend => {
                    if let Some(webview) = self.tab_webview(tab_id) {
                        let weak = Rc::downgrade(self);
                        let _ = wv::try_suspend(&webview, move |succeeded| {
                            let Some(app) = weak.upgrade() else { return };
                            let mut tabs = app.tabs.borrow_mut();
                            if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                                // A refused suspend leaves the tab at LowMemory
                                // so the next tick tries again rather than
                                // believing a freeze that never happened.
                                tab.power = if succeeded {
                                    TabPower::Suspended
                                } else {
                                    TabPower::LowMemory
                                };
                            }
                            drop(tabs);
                            app.mark_tabs();
                        });
                    }
                }
                ReclaimAction::Discard => {
                    let mut tabs = self.tabs.borrow_mut();
                    if let Some(tab) = tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.discard();
                    }
                    drop(tabs);
                    self.mark_tabs();
                }
            }
        }
    }
}

impl WindowDelegate for App {
    fn on_resize(&self, _width: i32, _height: i32, _dpi: u32) {
        self.layout();
    }

    fn on_flush_ui(&self) {
        self.flush_ui();
    }

    fn on_minimized(&self, minimized: bool) {
        self.minimized.set(minimized);

        if minimized {
            // Nothing is on screen, so every renderer can trim immediately
            // rather than waiting for the next tick; the policy then suspends
            // them on schedule.
            let tabs = self.tabs.borrow();
            for tab in tabs.iter() {
                if let Some(webview) = &tab.webview {
                    let _ = wv::set_low_memory(webview, true);
                }
            }
            drop(tabs);

            self.flush_counters();
            self.storage.release_memory();
            procstats::trim_working_set();
        }

        // Restoring has to re-enable the timer even when only one tab is open,
        // because the foreground tab may need waking up.
        self.update_reclaim_timer();
        if let Some(app) = self.me() {
            app.reclaim();
        }
    }

    fn on_reclaim_tick(&self) {
        // Cheap, and it bounds how much counter progress a crash can lose when
        // the new-tab page is not open to trigger a flush of its own.
        self.flush_counters();
        if let Some(app) = self.me() {
            app.reclaim();
        }
    }

    fn on_filters_ready(&self) {
        let Some(compiled) = self.refresh_slot.take() else {
            return;
        };
        if self.blocker.borrow_mut().replace_engine(&compiled.engine) {
            self.toast(
                format!("Filter lists updated ({} sources)", compiled.rule_sources),
                ToastKind::Info,
            );
        }
    }

    fn on_maximize_changed(&self, _maximized: bool) {
        self.push_window_mode();
    }

    fn on_close(&self) -> bool {
        // Incognito state lives only in memory, so there is nothing to flush.
        if !self.incognito {
            self.flush_counters();
            self.save_settings();
        }
        true
    }
}

/// Load the best blocker we can without blocking startup on a network fetch.
fn load_blocker(paths: &Paths, settings: &Settings) -> Blocker {
    if !settings.adblock_enabled {
        let mut blocker = Blocker::empty();
        blocker.set_enabled(false);
        return blocker;
    }

    if let Some(blocker) = lists::load_cached(&paths.filter_cache) {
        return blocker;
    }
    // No compiled cache: fall back to whatever raw lists are on disk. Still
    // much cheaper than a network round trip, and the background refresh will
    // replace this shortly.
    let local = lists::load_local_lists(&paths.filter_lists);
    if local.is_empty() {
        Blocker::empty()
    } else {
        Blocker::compile(local)
    }
}

/// Accept only a syntactically valid CSS hex colour from page content.
///
/// The value is interpolated into the chrome's stylesheet, so anything else
/// would let a page inject CSS into our UI.
fn normalise_theme_color(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let hex = value.strip_prefix('#')?;
    let valid_len = matches!(hex.len(), 3 | 4 | 6 | 8);
    if !valid_len || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", hex.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::normalise_theme_color;

    #[test]
    fn accepts_valid_hex_colours() {
        assert_eq!(normalise_theme_color("#1A2B3C").as_deref(), Some("#1a2b3c"));
        assert_eq!(normalise_theme_color("  #abc  ").as_deref(), Some("#abc"));
        assert_eq!(normalise_theme_color("#11223344").as_deref(), Some("#11223344"));
    }

    #[test]
    fn rejects_anything_that_could_inject_css() {
        // These would otherwise land verbatim in a style attribute.
        assert_eq!(normalise_theme_color("red; --surface-0: white"), None);
        assert_eq!(normalise_theme_color("url(javascript:alert(1))"), None);
        assert_eq!(normalise_theme_color("rgb(1,2,3)"), None);
        assert_eq!(normalise_theme_color("#12345"), None);
        assert_eq!(normalise_theme_color("#gggggg"), None);
        assert_eq!(normalise_theme_color(""), None);
        assert_eq!(normalise_theme_color("   "), None);
        // Named colours are valid CSS but not worth the parser.
        assert_eq!(normalise_theme_color("rebeccapurple"), None);
    }
}
