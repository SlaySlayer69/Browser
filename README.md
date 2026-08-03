# CleanDark

A minimal, memory-first desktop browser. Rust + Win32 + the WebView2 runtime —
no Electron, no bundled Chromium, no framework in the UI layer.

The browser ships seven features and nothing else: history, downloads, a native
content blocker, bookmarks, speed dials, an encrypted password vault, and
private windows.

---

## Why WebView2 rather than a bundled engine

WebView2 reuses the Edge runtime that is already on the machine. There is no
second Chromium on disk, and — because every tab shares one environment — one
browser process, one GPU process and one network service for the whole
application. Combined with `--process-per-site`, ten tabs on the same site
collapse into a single renderer.

The cost is that this is Windows-only by construction. The portable core
(blocker, storage, vault, settings, IPC) builds and tests on any host; only the
engine binding is gated behind `cfg(windows)`.

## Architecture

```
src/
  main.rs              entry point, STA init, fatal-error dialog
  config.rs            profile paths + settings (portable)
  util.rs              omnibox parsing, hashing, host extraction (portable)

  platform/            Win32
    window.rs            host window, custom frame, DPI, child hosts
    http.rs              WinHTTP GET for filter lists
    shell.rs             open / reveal a downloaded file

  engine/              WebView2
    flags.rs             Chromium command line          (portable + tested)
    resource.rs          resource-context classification (portable + tested)
    environment.rs       environment + controller creation
    webview.rs           per-WebView hardening, suspend/resume, asset mapping

  blocker/             content blocking
    mod.rs               engine + decision cache        (portable + tested)
    lists.rs             fetch, compile, cache          (portable + tested)

  browser/
    reclaim.rs           background-tab policy          (portable + tested)
    tab.rs               tab model
    app.rs               orchestration + command dispatch

  storage/             SQLite: history, downloads, bookmarks, speed dials
  vault/               Argon2id + XChaCha20-Poly1305 password store
  ipc/                 the typed UI <-> host protocol

ui/                    the chrome, rendered by its own WebView
```

### Two WebViews, side by side

The HTML chrome lives in its own WebView, sized to the header strip. It does
**not** overlap the page: the header and the viewport are two non-overlapping
child windows separated by a 1px solid line. There is no translucent layer over
the viewport at any time, so there is no steady-state overdraw.

The one exception is deliberate: while the omnibox dropdown or the menu is open,
the host grows the chrome host window over the page. The page is never resized,
so it does not reflow — it is simply occluded, the way a dropdown should be.

Each WebView2 controller gets its own child `HWND`. That is what makes the
overlay possible (z-order via `SetWindowPos`) and makes tab switching a
`ShowWindow` call rather than a resize.

## Memory behaviour

Three levers, applied by `browser/reclaim.rs` on a timer:

| Idle time | Action | Cost to undo |
|---|---|---|
| backgrounded | `SetMemoryUsageTargetLevel(LOW)` | none |
| past `suspend_after_secs` | `TrySuspend()` — document frozen, heap purged | `Resume()`, no reload |
| past `discard_after_secs` (aggressive only) | controller torn down | page reloads |

A tab playing audio never gets past the first row. A `TrySuspend` that the
runtime refuses leaves the tab marked as *not* suspended, so the next tick
retries instead of trusting a freeze that never happened.

Chromium flags live in `engine/flags.rs`. Two of them trade away a security
boundary (`disable_site_isolation`, `disable_smartscreen`); both default to
**keeping** the protection and are opt-in via `settings.json`.

> WebView2 silently ignores command-line switches it does not recognise, so a
> typo or a renamed Chromium feature fails invisibly. The list is deliberately
> short and confined to switches verified against Chromium; treat it as
> something to re-check when you bump the runtime, not as fire-and-forget.

## Content blocking

Brave's `adblock` crate (native Rust) parses EasyList-syntax rules. Two things
keep the per-request cost inside its budget:

* **Selective interception.** `WebResourceRequested` blocks the requesting
  renderer until our handler returns, so we register filters only for the
  contexts that actually carry ads and trackers. Stylesheets and fonts — high
  volume, negligible tracker share — are not intercepted at all.
* **A fixed-size decision cache.** 8192 direct-mapped slots, 128 KiB, allocated
  once. A hit is one hash and one array probe. Cache keys fold the *host* of the
  page rather than its full URL, so every subresource on a page shares entries.

Only network rules are compiled (`RuleTypes::NetworkOnly`); cosmetic rules would
cost tens of megabytes of resident memory for rules we never consult.

Main-frame navigations are decided in `NavigationStarting`, not in the resource
interceptor, so a blocked navigation leaves you on the current page instead of a
blank tab.

The compiled engine is cached on disk. Startup loads the cache in milliseconds;
a background thread refreshes the lists and hands back serialized bytes (the
engine itself is `!Send`).

## Password vault

Argon2id (19 MiB, t=2) derives a key from the master password;
XChaCha20-Poly1305 encrypts the whole entry list as one ciphertext. The 60-byte
header — including the KDF parameters — is authenticated as associated data, so
the cost factors cannot be downgraded without invalidating the tag.

This is a deliberately higher bar than Chrome's DPAPI-only storage, which any
process running as the same user can decrypt — the exact weakness infostealers
exploit. It does **not** defend against a keylogger or a debugger attached while
the vault is unlocked.

Nothing else touches it: WebView2's own autofill and password autosave are
disabled at the profile level.

## Design language

Every animated property is `opacity` or `transform` — both composite on the GPU
and never trigger layout or paint. Luminance changes use a solid pseudo-element
whose *opacity* animates, scoped to the control being hovered.

| Effect | Technique |
|---|---|
| Header/viewport separation | 1px solid `--divider`, no gradient, no shadow |
| Tab hover | white sheet, `opacity 0 → 0.07` |
| Omnibox focus | black sheet `opacity → 0.38` (inset) + accent ring `opacity → 1` |
| Icon hover / press | `opacity → 1` + `scale(1.05)` / `scale(0.95)` |
| Speed dial hover | `translateY(-3px)` + luminance lift |
| Chameleon tint | `<meta name="theme-color">` only — no pixel sampling |

The chameleon tint is **opt-in** and the colour is validated as a hex literal in
Rust before it reaches CSS; a page cannot inject arbitrary CSS through it.

Window dragging uses WebView2's non-client region support (`app-region: drag`),
so the host never has to guess which header pixels are a button.

## Security notes

* The chrome WebView has no host objects and no bridge beyond the typed
  `ipc::Command` enum. Unknown or malformed messages are dropped.
* Any page can call `postMessage`. Messages are only honoured as commands when
  `args.Source()` is our own asset origin; everything else may report a theme
  colour and nothing more.
* Internal pages set `Content-Security-Policy: default-src 'none'` with
  `'self'` for scripts and styles. All list rendering goes through
  `textContent`; nothing from history, a filename or a vault entry is parsed as
  HTML.
* Filter lists are fetched over HTTPS only — they steer what the browser blocks.

## Building

Requires the WebView2 runtime (preinstalled on Windows 11 and current Windows
10; otherwise the Evergreen installer).

```bash
cargo build --release --target x86_64-pc-windows-msvc
```

`build.rs` stages `ui/` next to the executable; the two must ship together.

Run the portable core's tests on any host:

```bash
cargo test
```

## What is verified, and what is not

This matters, so it is stated plainly.

**Verified in CI-equivalent conditions:** 102 unit tests pass, covering the
blocker (against real EasyList syntax, including exception rules, per-site
allowlisting and cache invalidation), the vault (tamper detection, nonce
freshness, no plaintext in the file), all four storage tables, the reclaim
policy, omnibox parsing, the Chromium flag builder, and the JSON key contract
between Rust and the UI.

**Type-checked against the real Win32 and WebView2 APIs** for
`x86_64-pc-windows-gnu`, so every COM signature, interface cast and event
handler in this repository is checked by the compiler, not written from memory.

**Not verified:** the browser has never been run. There is no Windows machine in
the loop, so nothing here has been executed against a live WebView2 runtime.
The parts most likely to need adjustment on first run are, in order:

1. **The custom frame** (`WM_NCCALCSIZE` / `WM_NCHITTEST` in
   `platform/window.rs`) — resize borders, maximize insets and snap behaviour
   are notoriously fiddly and cannot be checked without a compositor.
2. **Z-order and the dropdown overlay** — the child-host arrangement is sound in
   principle, but the exact `SetWindowPos` ordering may need a pass.
3. **`app-region: drag`** — requires `IsNonClientRegionSupportEnabled` and a
   recent enough runtime; on older runtimes the header will not drag the window.

## First run on Windows

The profile lives in `%LOCALAPPDATA%\CleanDark\data`:

| File | Contents |
|---|---|
| `settings.json` | everything in `config::Settings` (written on first change/exit) |
| `cleandark.sqlite` | history, downloads, bookmarks, speed dials |
| `vault.bin` | the encrypted password vault |
| `filters.bin` | the compiled adblock engine |
| `lists/` | raw filter lists as downloaded |
| `webview2/` | WebView2's own cache, cookies and local storage |

Deleting that folder is a full reset.

Two things about the first launch specifically:

* **Blocking is not active immediately.** There is no compiled engine yet, so a
  background thread fetches ~4 MB of filter lists and swaps the engine in when
  it is done. A toast reports how many sources were compiled. Every later launch
  loads `filters.bin` in milliseconds.
* **Tab suspension is easiest to observe with a shorter threshold.** Set
  `"suspend_after_secs": 15` in `settings.json` and restart; background tabs
  then dim in the strip within about half a minute, and Task Manager shows the
  renderer's working set drop.

### Debugging the UI

DevTools are compiled in for debug builds only. Page content can be inspected
with the normal right-click menu, but the *chrome* WebView deliberately has no
context menu and no browser accelerator keys, so F12 does not reach it. To
inspect the chrome, add `--remote-debugging-port=9222` to `SWITCHES` in
`engine/flags.rs`, rebuild, and open `http://localhost:9222` in Edge.

## Possible next steps

The two obvious ones:

* **Per-site autofill.** The vault already stores and matches credentials by
  normalised host, so offering them on a login form is UI work, not crypto work
  — a form-detection hook in the injected bridge script plus a small dropdown.
* **Session restore.** Tabs are not persisted across restarts at all. The tab
  model already carries everything needed (URL, title, order); it needs a table
  and a write on close.

Further out:

* **Cosmetic filtering.** Would remove leftover ad placeholders, at a real
  memory cost — worth making a setting rather than a default.
* **`prefers-reduced-transparency`** and a high-contrast pass on the tokens.
