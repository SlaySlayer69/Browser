# CleanDark verstehen

Ein Lerndokument für dieses Projekt. Es erklärt nicht Rust im Allgemeinen,
sondern **diesen Code** — jedes Konzept taucht dort auf, wo es im Projekt
tatsächlich gebraucht wird, mit Dateiname und Zeilenkontext.

Rund 8.000 Zeilen Rust plus 2.500 Zeilen HTML/CSS/JS. Das klingt viel, ist aber
in etwa zehn Konzepte aufgeteilt, die sich ständig wiederholen. Wenn du die
zehn hast, kannst du überall im Projekt lesen.

---

## Inhalt

1. [Wie du dieses Dokument benutzt](#1-wie-du-dieses-dokument-benutzt)
2. [Der große Überblick: Was passiert beim Laden einer Seite?](#2-der-große-überblick)
3. [Warum der Browser so gebaut ist](#3-warum-der-browser-so-gebaut-ist)
4. [Rust-Konzepte am echten Code](#4-rust-konzepte-am-echten-code)
5. [Windows-Konzepte](#5-windows-konzepte)
6. [Lesereihenfolge durch den Code](#6-lesereihenfolge-durch-den-code)
7. [Wie hier getestet wird](#7-wie-hier-getestet-wird)
8. [Performance-Denken](#8-performance-denken)
9. [Deine ersten Änderungen](#9-deine-ersten-änderungen)
10. [Glossar](#10-glossar)

---

## 1. Wie du dieses Dokument benutzt

Lies Kapitel 2 und 3 einmal am Stück — danach weißt du, wo was liegt. Kapitel 4
und 5 sind Nachschlagewerk: lies sie, wenn dir ein Konstrukt im Code begegnet,
das du nicht kennst. Kapitel 9 sind Übungen, die aufeinander aufbauen.

Halte den Code offen. Jeder Abschnitt nennt die Datei, um die es geht.

**Ein Hinweis vorweg:** Der Browser wurde nie ausgeführt — er ist vollständig
typgeprüft und die Logik ist getestet, aber niemand hat ihn je gestartet. Wenn
beim ersten Start etwas nicht stimmt, ist das erwartet und kein Zeichen, dass du
etwas falsch gemacht hast. Die Stellen mit dem höchsten Risiko stehen im README
unter „What is verified, and what is not".

---

## 2. Der große Überblick

### Die Frage, die alles erklärt

**Was passiert, wenn du `heise.de` in die Adressleiste tippst und Enter drückst?**

Folge dem Weg einmal komplett. Jeder Schritt nennt die Datei.

```
 1. Du tippst.                                    ui/chrome.js
    Die Adressleiste ist ein <input> in einer HTML-Seite,
    die in einer eigenen WebView läuft.

 2. Enter → commit()                              ui/chrome.js
    Schickt { cmd: "omniboxSubmit", text: "heise.de" }
    über window.chrome.webview.postMessage.

 3. Der Host empfängt die Nachricht.              src/browser/app.rs
    wire_chrome_events() hat dafür einen Handler registriert.

 4. Die Nachricht wird geparst.                   src/ipc/mod.rs
    Command::parse() → serde macht daraus ein
    Command::OmniboxSubmit { text, new_tab }.
    Unbekanntes wird verworfen.

 5. Text → URL                                    src/util.rs
    omnibox_to_url() entscheidet: "heise.de" hat einen Punkt
    und kein Leerzeichen → https://heise.de.
    "rust webview memory" wäre eine Suche geworden.

 6. Navigation starten.                           src/engine/webview.rs
    wv::navigate() ruft ICoreWebView2::Navigate auf.

 7. WebView2 meldet NavigationStarting.           src/browser/app.rs
    Wir prüfen, ob die URL selbst blockiert ist,
    merken sie als pending_main_frame und markieren die UI.

 8. Die Seite lädt Unterressourcen.               src/browser/app.rs
    Für jedes Script, Bild, XHR ... feuert
    WebResourceRequested — synchron, der Renderer wartet.

 9. Blockieren oder nicht?                        src/blocker/mod.rs
    should_block() schaut erst in einen Cache,
    nur bei einem Fehltreffer in die Filter-Engine.

10. Seite fertig.                                 src/browser/app.rs
    NavigationCompleted → Verlauf schreiben.       src/storage/history.rs

11. UI aktualisieren.                             src/browser/pending.rs
    Alle Events aus 7–10 haben nur "markiert".
    Jetzt geht **eine** Nachricht an die Chrome-WebView.

12. Die UI zeichnet.                              ui/chrome.js
    Tab-Titel, Adressleiste, Schild-Badge.
```

Wenn du diesen Ablauf verstanden hast, verstehst du das Projekt. Alles andere
sind Varianten davon.

### Die Schichten

```
┌─────────────────────────────────────────────────────────┐
│  ui/            HTML + CSS + JS   (die sichtbare UI)    │
├─────────────────────────────────────────────────────────┤
│  src/ipc/       Das Protokoll dazwischen                │
├─────────────────────────────────────────────────────────┤
│  src/browser/   Die Orchestrierung (Tabs, Kommandos)    │
├──────────────────────┬──────────────────────────────────┤
│  src/engine/         │  src/blocker/  src/storage/      │
│  WebView2-Anbindung  │  src/vault/    src/stats/        │
├──────────────────────┴──────────────────────────────────┤
│  src/platform/  Win32: Fenster, HTTP, Prozess-Metriken  │
└─────────────────────────────────────────────────────────┘
```

Faustregel: **je weiter unten, desto plattformabhängiger.** `src/platform/` und
`src/engine/environment.rs` laufen nur auf Windows. `src/blocker/`,
`src/storage/`, `src/vault/`, `src/util.rs`, `src/config.rs` laufen überall —
deshalb kannst du sie auf jedem Rechner testen.

---

## 3. Warum der Browser so gebaut ist

### 3.1 Warum kein eigenes Chromium?

Ein Browser braucht eine Engine, die HTML rendert. Drei Optionen:

| Ansatz | Was du mitlieferst | RAM |
|---|---|---|
| Electron | Komplettes Chromium + Node | sehr hoch |
| CEF | Komplettes Chromium | hoch |
| **WebView2** | **nichts** — nutzt das Edge auf dem PC | niedrig |

WebView2 ist die Engine, die in Windows ohnehin installiert ist. Wir starten
keine zweite Kopie, sondern bitten die vorhandene, für uns zu rendern.

**Der Preis:** Es gibt WebView2 nur auf Windows. Deshalb ist dieses Projekt
Windows-only — das ist keine Bequemlichkeit, sondern folgt zwingend aus der
Wahl der Engine.

### 3.2 Was ist überhaupt ein Prozess hier?

Wenn der Browser läuft, siehst du im Task-Manager mehrere Einträge:

```
cleandark.exe            ← unser Rust-Programm, klein (~10-20 MB)
msedgewebview2.exe       ← Browser-Prozess (Koordination)
msedgewebview2.exe       ← GPU-Prozess
msedgewebview2.exe       ← Netzwerk-Dienst
msedgewebview2.exe       ← Renderer für Site A
msedgewebview2.exe       ← Renderer für Site B
```

Unser Programm ist der **kleinste** Teil davon. Es rendert nichts. Es sagt der
Engine, was sie tun soll, und entscheidet, welche Requests durchdürfen.

Das erklärt auch, warum `src/platform/procstats.rs` eine Liste von Prozess-IDs
entgegennimmt: „Wie viel RAM braucht der Browser" lässt sich nur beantworten,
wenn man alle diese Prozesse zusammenzählt.

### 3.3 Warum zwei WebViews?

Die Browser-Oberfläche (Tab-Leiste, Adressleiste) ist selbst eine HTML-Seite in
einer eigenen WebView. Die Webseite läuft in einer zweiten.

```
┌──────────────────────────────┐
│  Chrome-WebView              │ ← ui/chrome.html
│  Tabs, Adressleiste, Menü    │
├──────────────────────────────┤ ← 1px Trennlinie
│                              │
│  Content-WebView             │ ← heise.de
│  die eigentliche Seite       │
│                              │
└──────────────────────────────┘
```

**Warum nicht eine WebView mit der Seite in einem iframe?** Weil die Seite dann
im selben Renderer wie unsere UI liefe. Ein Absturz oder ein Exploit auf der
Seite hätte Zugriff auf die Browser-Oberfläche. Getrennte WebViews heißt
getrennte Prozesse.

**Wichtig:** Die beiden überlappen sich **nicht**. Sie sind zwei nebeneinander
liegende Kindfenster. Das spart Overdraw (das GPU-Zusammenrechnen halbdurch-
sichtiger Ebenen). Nur wenn ein Dropdown aufgeht, wächst die Chrome-WebView
kurzzeitig über die Seite.

---

## 4. Rust-Konzepte am echten Code

### 4.1 Ownership: Wem gehört ein Wert?

Das Kernkonzept von Rust. Jeder Wert hat **genau einen Besitzer**. Wenn der
Besitzer verschwindet, wird der Wert freigegeben. Keine Garbage Collection,
kein manuelles `free`.

Aus `src/blocker/mod.rs`:

```rust
pub fn compile(lists: Vec<String>) -> Self {
    let mut set = FilterSet::new(false);
    for list in lists {          // `lists` wird hier verbraucht
        set.add_filter_list(list, opts);   // jeder String wandert weiter
    }
    ...
}
```

`Vec<String>` **by value** heißt: Der Aufrufer gibt die Listen ab. Danach kann
er sie nicht mehr benutzen — der Compiler verbietet es.

Vorher stand hier `lists: &[String]` (eine Ausleihe) und im Rumpf
`list.clone()`. Das war ein echter Fehler: `add_filter_list` will einen `String`
besitzen, also musste jede Liste kopiert werden — rund 8 MB umsonst.

**Lehre:** Wenn eine Funktion den Wert ohnehin behalten will, nimm ihn `by
value`. Eine Referenz zu nehmen und dann zu klonen ist schlechter als beides.

### 4.2 Borrowing: Ausleihen statt besitzen

Meistens will man einen Wert nur *anschauen*. Dafür gibt es Referenzen:

- `&T` — geteilte Ausleihe. Beliebig viele gleichzeitig, nur lesen.
- `&mut T` — exklusive Ausleihe. Genau eine, darf schreiben.

Diese Regel ist der Grund, warum es in Rust keine Data Races gibt.

Aus `src/browser/app.rs`, dem heißesten Pfad im ganzen Projekt:

```rust
let filter_type = {
    let tabs = self.tabs.borrow();              // geteilte Ausleihe
    let Some(tab) = tabs.iter().find(|t| t.id == tab_id) else { return };

    let blocked = self.blocker.borrow_mut().should_block(
        &uri,
        &tab.url,      // Referenz — keine Kopie
        &tab.host,     // Referenz — keine Kopie
        filter_type,
    );
    ...
};   // hier endet die Ausleihe
```

Vorher stand da `tab.url.clone()` — zwei Heap-Allokationen pro abgefangenem
Request. Bei ~300 Requests pro Seite sind das 600 Allokationen umsonst.

**Merke:** `&` kostet nichts. `.clone()` auf einem `String` kostet eine
Allokation plus Kopie. Wenn du `.clone()` schreibst, frag dich, ob eine Referenz
reicht.

### 4.3 `Option` und `Result`: kein `null`, keine Exceptions

Rust hat kein `null`. Was fehlen kann, ist ein `Option<T>`:

```rust
pub fn host_of(url: &str) -> Option<String>   // src/util.rs
```

Was schiefgehen kann, ist ein `Result<T, E>`:

```rust
pub fn open(path: &Path) -> rusqlite::Result<Self>   // src/storage/mod.rs
```

Der Compiler zwingt dich, beide Fälle zu behandeln. Drei Schreibweisen kommen
hier ständig vor:

```rust
// 1) `?` — Fehler nach oben weiterreichen
let conn = Connection::open(path)?;

// 2) let-else — bei None/Err früh aussteigen
let Some(tab) = tabs.iter().find(|t| t.id == id) else { return };

// 3) unwrap_or / unwrap_or_default — Ersatzwert
let host = util::host_of(url).unwrap_or_default();   // "" wenn unparsebar
```

Du wirst im Projekt fast nie `.unwrap()` sehen (das würde bei einem Fehler das
Programm abstürzen lassen) — außer in Tests, wo genau das gewünscht ist.

### 4.4 `Rc`, `Weak`, `RefCell`: geteilter Zustand mit einem Thread

Hier wird es interessant, und hier steckt der lehrreichste Bug des Projekts.

Das Problem: Die `App` muss aus vielen Event-Handlern erreichbar sein. Aber
Ownership sagt „genau ein Besitzer". Lösung:

**`Rc<T>`** — *Reference Counted*. Mehrere Besitzer, ein Zähler. Fällt der
Zähler auf 0, wird freigegeben.

```rust
let app = Rc::new(App { ... });          // Zähler: 1
let another = app.clone();               // Zähler: 2 — kein Deep Copy!
```

**`RefCell<T>`** — erlaubt Ändern durch eine `&`-Referenz. Die Borrow-Regeln
werden zur *Laufzeit* geprüft statt zur Compile-Zeit:

```rust
tabs: RefCell<Vec<Tab>>,

self.tabs.borrow()        // wie &Vec<Tab>
self.tabs.borrow_mut()    // wie &mut Vec<Tab>  — panict, wenn schon geliehen!
```

**`Weak<T>`** — ein Zeiger, der den Zähler **nicht** erhöht. Muss vor Gebrauch
mit `.upgrade()` geprüft werden.

Und jetzt der Punkt. Ein Handler, der die App braucht:

```rust
// FALSCH — würde einen Zyklus bauen:
let app = self.clone();                  // Rc, Zähler +1
webview.add_SourceChanged(... move |_, _| { app.tun_was(); });
//  App → hält WebView → hält Handler → hält App
//  Der Zähler fällt nie auf 0. Nichts wird je freigegeben.

// RICHTIG:
let weak = Rc::downgrade(self);          // Weak, Zähler unverändert
webview.add_SourceChanged(... move |_, _| {
    let Some(app) = weak.upgrade() else { return Ok(()) };
    app.tun_was();
});
```

Das ist die klassische Zyklus-Falle bei Referenzzählung, und sie ist der Grund,
warum im ganzen Projekt konsequent `Rc::downgrade` in Handlern steht.

> **Der Bug, der genau das war:** In einer früheren Version haben die Handler
> zusätzlich eine Kopie des *WebView-Interfaces* gecaptured, auf dem sie selbst
> registriert waren. Gleicher Zyklus, nur auf COM-Ebene statt Rust-Ebene: Der
> WebView hielt den Handler, der Handler hielt den WebView. Pro Tab ein
> geleaktes Objekt. Die Lösung war, den `sender`-Parameter zu benutzen, den der
> Handler ohnehin bekommt. Siehe Kapitel 5.3.

### 4.5 Traits: Verhalten ohne Vererbung

Ein Trait ist eine Liste von Methoden, die ein Typ bereitstellen kann. Aus
`src/platform/window.rs`:

```rust
pub trait WindowDelegate {
    fn on_resize(&self, width: i32, height: i32, dpi: u32);
    fn on_flush_ui(&self);
    fn on_minimized(&self, minimized: bool);
    fn on_close(&self) -> bool;
    ...
}
```

`src/browser/app.rs` implementiert ihn:

```rust
impl WindowDelegate for App {
    fn on_resize(&self, _w: i32, _h: i32, _dpi: u32) { self.layout(); }
    ...
}
```

**Warum?** Damit `window.rs` nichts über `App` wissen muss. Es kennt nur „irgend
etwas, das diese Methoden hat". Man könnte die App austauschen, ohne die
Fensterschicht anzufassen — und beim Lesen musst du nicht zwei Module
gleichzeitig im Kopf haben.

### 4.6 `Cow`: mal geliehen, mal besessen

`Cow` heißt *Clone on Write*. Ein Wert, der entweder eine Referenz oder ein
eigener Wert ist. Aus `src/util.rs`:

```rust
pub fn elide(text: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    if text.len() <= max_chars || text.chars().count() <= max_chars {
        return std::borrow::Cow::Borrowed(text);      // keine Allokation
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    std::borrow::Cow::Owned(out)                      // eine Allokation
}
```

Der häufige Fall (Titel ist schon kurz genug) kostet nichts. Nur der seltene
Fall allokiert. Vorher gab die Funktion immer `String` zurück — also immer eine
Allokation, auch wenn nichts zu tun war.

### 4.7 Lifetimes: wie lange lebt eine Referenz?

Das `'a` in `src/ipc/mod.rs`:

```rust
pub enum Event<'a> {
    Navigation {
        url: &'a str,      // geliehen — der Event kopiert die URL nicht
        title: &'a str,
        ...
    },
}
```

`'a` sagt dem Compiler: „Dieser Event darf nicht länger leben als die Strings,
auf die er zeigt." Da der Event sofort zu JSON serialisiert und weggeworfen
wird, ist das genau richtig — und spart zwei Kopien pro Event.

Lifetimes sind kein Laufzeit-Mechanismus. Sie verschwinden beim Kompilieren
vollständig. Sie sind nur eine Prüfung.

### 4.8 `#[cfg(windows)]`: bedingte Kompilierung

```rust
#[cfg(windows)]
pub mod app;
```

Alles darunter existiert nur, wenn für Windows gebaut wird. So kann der
portable Kern (Blocker, Storage, Vault) auf jedem Rechner kompiliert und
getestet werden, während die WebView2-Anbindung Windows-only bleibt.

Deshalb funktioniert `cargo test` auch auf einem Linux-Rechner — es testet dann
eben nur den portablen Teil.

---

## 5. Windows-Konzepte

Das hier ist der ungewohnteste Teil, wenn du aus der Web- oder Skript-Welt
kommst. Es lohnt sich, weil dieselben Konzepte in *jeder* nativen Windows-App
vorkommen.

### 5.1 Die Message Loop

Eine Windows-Anwendung ist im Kern eine Endlosschleife, die Nachrichten aus
einer Warteschlange holt. `src/platform/window.rs`:

```rust
pub fn run_message_loop() {
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);     // ruft die WndProc auf
    }
}
```

Jeder Mausklick, jede Tastatureingabe, jede Größenänderung ist eine Nachricht.
`GetMessageW` **blockiert**, wenn nichts da ist — deshalb verbraucht ein
untätiges Fenster keine CPU.

Das ist auch der Grund, warum das UI-Coalescing (Kapitel 8.3) funktioniert:
`PostMessage` hängt eine Nachricht hinten an. Alles, was schon in der Schlange
steht, wird vorher abgearbeitet.

### 5.2 Die WndProc

Die Funktion, die Nachrichten entgegennimmt:

```rust
unsafe extern "system" fn wnd_proc(
    hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SIZE => { ... }
        WM_CLOSE => { ... }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
```

Drei Dinge fallen auf:

- **`extern "system"`** — Windows ruft diese Funktion auf, also muss sie
  Windows' Aufrufkonvention benutzen, nicht Rusts.
- **`unsafe`** — der Compiler kann hier nichts garantieren.
- **`_ => DefWindowProcW(...)`** — alles, was uns nicht interessiert, macht
  Windows selbst. Das ist wichtig: lässt man das weg, funktioniert das Fenster
  nicht mehr.

**Wie kommt die App in die WndProc?** Windows kennt nur das `HWND`. Der Trick
steht in `WM_NCCREATE`: Beim Erzeugen des Fensters übergeben wir einen Zeiger
auf unseren Zustand, holen ihn dort ab und speichern ihn mit
`SetWindowLongPtrW(hwnd, GWLP_USERDATA, ...)` am Fenster. Danach kommt man in
jeder Nachricht mit `GetWindowLongPtrW` wieder heran.

### 5.3 COM und Referenzzählung

WebView2 ist eine **COM**-Bibliothek. COM ist Microsofts Weg, Objekte zwischen
Sprachen zu teilen. Für dich sind zwei Dinge wichtig:

**Erstens: Referenzzählung.** Jedes COM-Objekt zählt seine Referenzen. Bei 0
wird es freigegeben. Das `windows`-Crate macht das automatisch — ein Klon
erhöht, ein Drop verringert.

**Zweitens: Zyklen werden nicht erkannt.** Genau wie bei `Rc`. Und genau das
war der Bug:

```rust
// FALSCH — der geleakte Zyklus
let op = operation.clone();                     // COM-Zähler +1
operation.add_BytesReceivedChanged(... move |_sender, _args| {
    let received = read_i64(|out| op.BytesReceived(out));
    ...
});
// operation → hält Handler → hält operation. Zähler nie 0.
```

```rust
// RICHTIG — der Handler bekommt das Objekt ohnehin geliefert
operation.add_BytesReceivedChanged(... move |sender, _args| {
    let Some(op) = sender else { return Ok(()) };
    let received = read_i64(|out| op.BytesReceived(out));
    ...
});
```

**Lehre, die überall gilt:** Wenn ein Callback auf einem Objekt registriert
wird und dieses Objekt braucht, nimm den Parameter, den der Callback bekommt.
Capture das Objekt nicht.

### 5.4 Out-Parameter

C-APIs geben Werte oft über Zeiger zurück statt über den Rückgabewert (der ist
für den Fehlercode reserviert):

```rust
// Nicht: let can_go_back = webview.CanGoBack();
let mut value = BOOL::default();
webview.CanGoBack(&mut value)?;      // schreibt in `value`
```

Weil das im Projekt oft vorkommt, gibt es kleine Helfer in `src/browser/app.rs`:

```rust
fn read_bool<F>(get: F) -> bool
where F: FnOnce(*mut BOOL) -> Result<()> {
    let mut value = BOOL::default();
    get(&mut value).is_ok() && value.as_bool()
}

// Benutzung:
let back = read_bool(|out| sender.CanGoBack(out));
```

Das ist auch ein gutes Beispiel dafür, wie man eine hässliche API einmal
einpackt statt zwanzigmal zu wiederholen.

---

## 6. Lesereihenfolge durch den Code

Nicht alphabetisch lesen. In dieser Reihenfolge:

| # | Datei | Zeilen | Was du hier lernst |
|---|---|---:|---|
| 1 | `src/util.rs` | ~150 | Der sanfte Einstieg: reine Funktionen, `Option`, `Cow`, Tests |
| 2 | `src/config.rs` | ~200 | serde, Defaults, Dateien atomar schreiben |
| 3 | `src/browser/reclaim.rs` | ~250 | Wie man Logik testbar macht: eine reine Funktion für die ganze Policy |
| 4 | `src/browser/pending.rs` | ~150 | Ein kleines Modul mit einer klaren Idee |
| 5 | `src/blocker/mod.rs` | ~400 | Ein Cache von Hand, Hot-Path-Denken |
| 6 | `src/storage/history.rs` | ~200 | SQL in Rust, Upsert, Tests gegen echte DB |
| 7 | `src/ipc/mod.rs` | ~450 | Das Protokoll — hier siehst du **alles**, was der Browser kann |
| 8 | `src/vault/mod.rs` | ~450 | Angewandte Kryptographie, ein Binärformat |
| 9 | `src/platform/window.rs` | ~450 | Win32 pur: Message Loop, WndProc |
| 10 | `src/engine/environment.rs` | ~200 | COM-Aufrufe, asynchrone Erzeugung |
| 11 | `src/browser/app.rs` | ~1500 | Wo alles zusammenläuft. Zuletzt lesen! |

**`app.rs` ist mit Abstand die größte Datei.** Lies sie nicht am Stück. Sie ist
in Abschnitte gegliedert (`// ---- tabs`, `// ---- commands`, …). Nimm dir
einen Abschnitt vor.

### Die UI

| Datei | Was |
|---|---|
| `ui/theme.css` | Design-Tokens. Hier lernst du, warum nur `opacity` und `transform` animiert werden |
| `ui/chrome.js` | Die Browser-Oberfläche. Kein Framework — bewusst |
| `ui/newtab.js` | Neuer Tab: Uhr, Speed Dials, Privacy Hub |
| `ui/pages/common.js` | Geteilte Helfer der internen Seiten |

---

## 7. Wie hier getestet wird

### 7.1 Die Grundidee: Logik von Plattform trennen

Der Browser lässt sich nicht automatisch testen — dafür bräuchte man Windows,
eine Anzeige und einen echten WebView. Also wird die **Entscheidung** von der
**Ausführung** getrennt.

Beispiel `src/browser/reclaim.rs`. Statt in `app.rs` mitten im Timer-Handler zu
entscheiden, wann ein Tab schlafen gelegt wird, gibt es eine reine Funktion:

```rust
pub fn decide(tab: TabState, minimized: bool, settings: &Settings) -> ReclaimAction
```

Rein heißt: gleiche Eingabe → gleiche Ausgabe, keine Seiteneffekte. Das kann
man auf jedem Rechner testen:

```rust
#[test]
fn a_minimized_window_reclaims_its_foreground_tab_too() {
    let active = TabState { is_active: true, ..tab(300, TabPower::Normal) };
    assert_eq!(decide(active, false, &balanced()), ReclaimAction::None);
    assert_eq!(decide(active, true,  &balanced()), ReclaimAction::Suspend);
}
```

`app.rs` **führt** die Entscheidung dann nur noch aus. Diese Trennung ist der
wichtigste Testtrick im ganzen Projekt.

### 7.2 Tests ausführen

```powershell
cargo test                    # alle 130 Tests
cargo test blocker            # nur die mit "blocker" im Namen
cargo test -- --nocapture     # println! sichtbar machen
```

### 7.3 Was der Compiler nicht sieht

Zwischen Rust und JavaScript gibt es Verbindungen, die kein Compiler prüft:
Element-IDs, Kommando-Namen, Event-Namen. Ein Tippfehler dort scheitert
lautlos. Dafür gibt es ein Skript:

```powershell
python tools\check-ui.py
```

Es prüft: existiert jede aus JS referenzierte ID im HTML? Ist jedes gesendete
Kommando eine echte `ipc::Command`-Variante? Parst jedes Skript?

### 7.4 Messen statt raten

```powershell
cargo test --release -- --ignored --nocapture
```

Diese Tests prüfen nichts, sie **messen**. Zum Beispiel, was eine
Blocker-Entscheidung kostet:

```
cache hit
  host precomputed (current)      68 ns/request
  host parsed per call (previous) 561 ns/request
  -> 8.2x faster
```

Das ist ein Werkzeug, das du benutzen solltest, bevor du „optimierst": Erst
messen, dann ändern, dann wieder messen.

---

## 8. Performance-Denken

Das Projekt ist um drei Ideen gebaut. Sie sind allgemein nützlich.

### 8.1 Finde den heißen Pfad

Nicht jeder Code ist gleich wichtig. `WebResourceRequested` läuft **pro
Unterressource** — bei einer Nachrichtenseite hunderte Male, und der Renderer
wartet währenddessen.

Die Konsequenzen im Code:

- Der Host der Seite wird **einmal pro Navigation** berechnet und am Tab
  gespeichert (`Tab::set_url`), nicht pro Request.
- Es gibt einen Cache mit fester Größe (8192 Einträge, 128 KiB), einmal
  allokiert. Ein Treffer ist ein Hash und ein Array-Zugriff.
- Auf diesem Pfad wird **nichts** allokiert und **nichts** in die Datenbank
  geschrieben.

**Übertragbar:** Frag bei jeder Codezeile „wie oft läuft das?". Eine Zeile, die
einmal pro Klick läuft, darf teuer sein. Eine, die 300× pro Seite läuft, nicht.

### 8.2 Batching: viele kleine Schreibvorgänge zu einem machen

Der Zähler für blockierte Requests soll dauerhaft gespeichert werden. Naiv wäre
ein `UPDATE` pro Block — hunderte Datenbankschreibvorgänge pro Seite.

`src/stats.rs`:

```rust
pub struct PendingCounters { pub requests: u64, pub bytes: u64 }
```

Die Zähler laufen im RAM auf und werden geschrieben, wenn 200 zusammengekommen
sind, wenn die Statistikseite fragt, oder beim Beenden. Bei einem Absturz gehen
höchstens 200 verloren — für eine Statistik völlig in Ordnung.

**Übertragbar:** Bei jedem „ich schreibe das jedes Mal weg" — muss es wirklich
jedes Mal sein? Was ist die schlimmste Folge, wenn ein bisschen verloren geht?

### 8.3 Coalescing: Arbeit erst am Ende tun

Bei einem Seitenaufbau feuert WebView2 einen Schwall Events. Jedes wollte sofort
die UI aktualisieren — neun Nachrichten für einen Zustand, von dem nur der
letzte je sichtbar wird.

`src/browser/pending.rs` dreht das um:

```rust
// statt sofort zu senden:
app.mark_tabs();

// Das erste Markieren postet EINE Nachricht an das eigene Fenster.
if was_empty && !pending.is_empty() {
    MainWindow::post_app_message(hwnd, WM_APP_FLUSH_UI);
}
```

Weil `PostMessage` sich hinten anstellt, ist der ganze Schwall zugestellt, bevor
die Flush-Nachricht drankommt. Neun Events → ein Update.

Das ist dasselbe Muster wie *invalidate/repaint* in jedem UI-Toolkit oder
*debouncing* in JavaScript. Entscheidend hier: **kein Timer**. Ein untätiger
Browser postet nichts und wacht für nichts auf.

---

## 9. Deine ersten Änderungen

Aufsteigend nach Schwierigkeit. Nach jeder Änderung: `cargo test` und
`cargo run`.

### Stufe 1 — Werte anfassen (5 Minuten)

**a) Akzentfarbe ändern.** In `ui/theme.css`:
```css
--accent: #4c8dff;     →    --accent: #ff8a4c;
```
Neu starten. Fokusring, Schild-Badge und Tab-Punkt ändern sich mit — weil alles
über dieses eine Token geht. Das ist der Sinn von Design-Tokens.

**b) Suchmaschine ändern.** In `src/config.rs`:
```rust
search_template: "https://duckduckgo.com/?q={q}".into(),
```
Achtung: `{q}` muss vorkommen, sonst weist die Validierung es zurück. Such nach
`SetSearchTemplate` in `app.rs`, um zu sehen wo.

**c) Tab-Schlafenszeit verkürzen.** In `src/config.rs` `suspend_after_secs` auf
`15`. Dann mehrere Tabs öffnen und im Task-Manager zusehen.

### Stufe 2 — Eine Kachel im Privacy Hub hinzufügen (30 Minuten)

Ziel: eine siebte Kachel, die anzeigt, wie viele Lesezeichen du hast.

Das übt den **kompletten Weg** von der Datenbank bis in die UI. Fünf Schritte:

1. **Datenbank** — `src/storage/bookmarks.rs`:
   ```rust
   pub fn bookmark_count(&self) -> rusqlite::Result<u64> {
       let mut stmt = self.conn().prepare_cached("SELECT COUNT(*) FROM bookmarks")?;
       let count: i64 = stmt.query_row([], |r| r.get(0))?;
       Ok(count.max(0) as u64)
   }
   ```
   Und gleich einen Test dazu.

2. **Datenstruktur** — `src/stats.rs`, Feld in `PrivacyStats`:
   ```rust
   pub bookmark_count: u64,
   ```

3. **Befüllen** — `src/browser/app.rs` in `push_privacy_stats()`:
   ```rust
   bookmark_count: self.storage.bookmark_count().unwrap_or(0),
   ```

4. **HTML** — `ui/newtab.html`, eine `.stat`-Kachel dazu (kopier eine
   bestehende, ändere `id` und Beschriftung).

5. **JS** — `ui/newtab.js` in `renderStats()`:
   ```js
   setStat("s-bookmarks", null, [String(stats.bookmarkCount), ""]);
   ```

**Achte auf Schritt 5:** Rust schreibt `bookmark_count`, JS liest
`bookmarkCount`. Das macht `#[serde(rename_all = "camelCase")]` automatisch.
Wenn du es vergisst, ist der Wert in JS `undefined` — und genau dafür gibt es
`python tools\check-ui.py` und die Tests in `src/ipc/mod.rs`.

### Stufe 3 — Ein neues Kommando (1 Stunde)

Ziel: ein Knopf „alle anderen Tabs schließen".

1. **`src/ipc/mod.rs`** — Variante zu `Command`:
   ```rust
   CloseOtherTabs { keep: u32 },
   ```
2. **`src/browser/app.rs`** — Fall in `handle_command`. Der Compiler zwingt
   dich dazu: `match` muss alle Varianten abdecken. **Das ist ein Feature** —
   du kannst gar nicht vergessen, das Kommando zu behandeln.
3. **`ui/chrome.js`** — Menüeintrag, der
   `send({ cmd: "closeOtherTabs", keep: state.active })` schickt.
4. **`python tools\check-ui.py`** — prüft, dass die Namen zusammenpassen.

### Stufe 4 — Session Restore (ein Wochenende)

Tabs überleben derzeit keinen Neustart. Was du brauchst:

- Eine Tabelle `session(id, url, title, position, active)` — schau dir die
  Migration in `src/storage/mod.rs` an, du brauchst `SCHEMA_VERSION = 3`.
- Schreiben in `on_close`.
- Lesen in `App::launch`, statt den einen Neuen-Tab zu öffnen.
- Ein Test, der den Round-Trip prüft.

Der interessante Teil ist die Migration: Ein bestehendes Profil hat die Tabelle
noch nicht. Schau dir an, wie `migrating_an_existing_v1_database_keeps_its_data`
das testet.

---

## 10. Glossar

| Begriff | Bedeutung |
|---|---|
| **Borrow** | Ausleihe. `&T` lesend, `&mut T` schreibend und exklusiv |
| **COM** | Component Object Model. Microsofts Objektmodell; WebView2 ist COM |
| **Cow** | *Clone on Write*. Wert, der geliehen oder besessen sein kann |
| **Coalescing** | Mehrere Änderungen zu einem Update zusammenfassen |
| **Crate** | Ein Rust-Paket. `adblock`, `rusqlite`, `windows` sind Crates |
| **HWND** | *Handle to a Window*. Windows' Kennung für ein Fenster |
| **Hot Path** | Code, der sehr oft läuft. Hier: `WebResourceRequested` |
| **IPC** | *Inter-Process Communication*. Hier: UI ↔ Host |
| **Lifetime** | Wie lange eine Referenz gültig ist. `'a` |
| **Ownership** | Jeder Wert hat genau einen Besitzer |
| **Rc / Weak** | Geteilter Besitz mit Zähler / Zeiger ohne Zähler |
| **RefCell** | Ändern durch `&`, Borrow-Prüfung zur Laufzeit |
| **Renderer** | Der Prozess, der eine Webseite tatsächlich zeichnet |
| **serde** | Rust-Bibliothek für Serialisierung. Macht Structs ↔ JSON |
| **Trait** | Eine Menge von Methoden, die ein Typ bereitstellt |
| **WebView2** | Die Edge-Engine als einbettbare Komponente |
| **WndProc** | Die Funktion, die Windows-Nachrichten verarbeitet |

---

## Wo du weiterliest

- **Das Rust-Buch** — <https://doc.rust-lang.org/book/>, auf Deutsch unter
  <https://rust-lernen.de>. Kapitel 4 (Ownership) und 15 (`Rc`, `RefCell`)
  sind für dieses Projekt die wichtigsten.
- **Rust by Example** — <https://doc.rust-lang.org/rust-by-example/>, gut zum
  Nachschlagen einzelner Konstrukte.
- **WebView2-Doku** — <https://learn.microsoft.com/microsoft-edge/webview2/>
- **Das README dieses Projekts** — erklärt die *Entscheidungen*; dieses
  Dokument erklärt die *Mechanik*.
- **Die Kommentare im Code.** Sie erklären durchgängig das *Warum*, nicht das
  *Was*. Wenn du dich fragst „warum ist das so komisch gemacht", steht die
  Antwort meistens direkt darüber.
