# CleanDark

A minimal, memory-first desktop browser. Rust + Win32 + the WebView2 runtime —
no Electron, no bundled Chromium, no framework in the UI layer.

The browser ships seven features and nothing else: history, downloads, a native
content blocker, bookmarks, speed dials, an encrypted password vault, and
private windows. The new-tab page adds a clock and a privacy hub on top of
them.

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

### Doing nothing well

Idle cost is a feature, so three mechanisms exist purely to avoid work:

* **Chrome updates are coalesced.** A page load fires a burst of WebView2
  events, and pushing each one to the chrome costs a JSON serialization, a
  cross-process message, a JS wakeup and a DOM reconcile. Handlers mark what
  changed (`browser/pending.rs`); the first mark posts one application message,
  which Windows dispatches after the burst has landed. Message-driven, not
  timer-driven — an idle browser posts nothing.
* **The reclaim timer stops when there is nothing to reclaim.** With a single
  visible tab the foreground tab is exempt anyway, so the timer would wake up
  every 15–60 s to decide nothing.
* **Resizes are throttled while the frame is dragged.** Windows sends `WM_SIZE`
  continuously, and each one forces a renderer relayout; between
  `WM_ENTERSIZEMOVE` and `WM_EXITSIZEMOVE` that is capped at one per 16 ms, with
  the exact final geometry applied once when the drag ends.

### Minimizing

A minimized window paints nothing, so its foreground tab loses its exemption
and is suspended on the normal schedule — audio still excepted, since
minimizing is how most people listen to it. The host also trims its own working
set, drops SQLite's caches and flushes pending counters at the same moment.

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

## The new-tab page

Clock and date, the search field, the speed dial grid, then a privacy hub:

| Tile | Source | Kind |
|---|---|---|
| Trackers & ads blocked | lifetime counter in SQLite | **measured** |
| Bandwidth saved | blocked count x per-type average | *estimated* |
| Time saved | blocked count and estimated bytes | *estimated* |
| Browser memory | summed private commit of every WebView2 process | **measured** |
| CPU right now | delta of process CPU time over the poll interval | **measured** |
| Blocked this session / processes | in-memory counter | **measured** |

### Why two of them are estimates, and how that is handled

A blocked request is never issued, so its response never exists and its size is
never known. Every browser showing "bandwidth saved" is multiplying a request
count by an assumed average; the honest thing is to say so rather than present
a fabricated number as a measurement.

So: the assumptions live in `src/stats.rs` as named constants with the
reasoning attached, the averages differ per resource type (a tracking pixel is
not a video), the model is deliberately conservative — a test pins 12.6k
blocked requests to single-digit minutes rather than the half-hour figures some
browsers advertise — and both estimated tiles carry a footnote in the UI saying
why they are estimates.

### Memory: private commit, not working set

`procstats.rs` sums `PrivateUsage` across every WebView2 process plus our own.
Summing working sets would double-count heavily, because Chromium processes
share a great deal of mapped memory, and the total would read far above what
the browser actually costs.

### Cost of the hub

The page polls the host every 2 s and **stops entirely when the tab is not
visible** — `visibilitychange` clears both the clock and the stats timer. A
background new-tab page that keeps ticking is exactly the idle cost this
browser exists to avoid, and nothing is sampled when no new-tab page is open.
The poll interval doubles as the CPU averaging window, since CPU time is a
counter and a rate needs two readings.

Blocked-request counters never touch the database on the hot path: they
accumulate in memory and flush when the hub polls, when 200 have piled up, on
the reclaim tick, and at shutdown.

Private windows report their session only, so an incognito hub never surfaces
the main profile's totals.

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

## Measuring

The interception path has a timing harness, because "the cache makes lookups
cheap" is a claim that should be checkable:

```bash
cargo test --release -- --ignored --nocapture hot_path
```

It reports the cost of a cached decision, a cache miss, and a blocked request.
On the machine this was developed on, a cached decision costs **69 ns** with the
page host precomputed versus **567 ns** when the source URL was parsed on every
call — the regression that measurement was written to catch.

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

**Verified in CI-equivalent conditions:** 119 unit tests pass, covering the
blocker (against real EasyList syntax, including exception rules, per-site
allowlisting and cache invalidation), the vault (tamper detection, nonce
freshness, no plaintext in the file), all five storage tables including the v1
to v2 migration, the reclaim policy, omnibox parsing, the Chromium flag
builder, the statistics model (including an overflow the suite caught), and the
JSON key contract between Rust and the UI.

`python3 tools/check-ui.py` covers what the compiler cannot see across the
Rust/JavaScript boundary: that every element id referenced from a script exists
in its page, that every command the UI sends is a real `ipc::Command` variant,
that every event it listens for is a real `ipc::Event` variant, and that every
script parses. Each of those fails silently at runtime otherwise — a typo'd id
yields `null`, an unknown command is dropped by `Command::parse`, an event
nobody listens for simply never renders.

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
