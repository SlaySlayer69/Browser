/* Browser chrome controller.
 *
 * No framework and no virtual DOM: the tab strip is the only list that changes
 * often, and it is small enough that a keyed reconcile by hand is both shorter
 * and cheaper than any library would be. Nothing here allocates per frame.
 */

(function () {
  "use strict";

  var host = window.chrome && window.chrome.webview;
  var send = function (message) {
    if (host) {
      host.postMessage(message);
    }
  };

  var el = function (id) {
    return document.getElementById(id);
  };

  var tabsEl = el("tabs");
  var urlEl = el("url");
  var omniboxEl = el("omnibox");
  var dropdownEl = el("dropdown");
  var menuEl = el("menu");
  var toastEl = el("toast");
  var shieldEl = el("shield");
  var shieldCountEl = el("shield-count");
  var starEl = el("star");
  var lockEl = el("lock");
  var backEl = el("back");
  var forwardEl = el("forward");

  var state = {
    tabs: [],
    active: null,
    url: "",
    editing: false,
    suggestions: [],
    selected: -1,
    settings: { adblock: true, chameleon: false, reduceMotion: false }
  };

  /* ------------------------------------------------------------ tab strip */

  /* Reconcile by id so a re-render never destroys the node the pointer is
   * hovering — otherwise the hover transition restarts on every update. */
  var tabNodes = new Map();

  function renderTabs() {
    var seen = new Set();

    state.tabs.forEach(function (tab, index) {
      seen.add(tab.id);
      var node = tabNodes.get(tab.id);

      if (!node) {
        node = document.createElement("div");
        node.className = "tab lift no-drag";
        node.innerHTML =
          '<span class="tab-dot"></span>' +
          '<span class="tab-title"></span>' +
          '<button class="icon-btn tab-close" tabindex="-1">' +
          '<svg viewBox="0 0 16 16"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/></svg>' +
          "</button>";

        node.addEventListener("pointerdown", function (event) {
          if (event.target.closest(".tab-close")) {
            return;
          }
          send({ cmd: "activateTab", id: tab.id });
        });
        node.querySelector(".tab-close").addEventListener("click", function (event) {
          event.stopPropagation();
          send({ cmd: "closeTab", id: tab.id });
        });
        // Middle click closes, as everywhere else.
        node.addEventListener("auxclick", function (event) {
          if (event.button === 1) {
            send({ cmd: "closeTab", id: tab.id });
          }
        });

        tabNodes.set(tab.id, node);
      }

      var title = node.querySelector(".tab-title");
      if (title.textContent !== tab.title) {
        title.textContent = tab.title;
        node.title = tab.url;
      }

      node.classList.toggle("active", tab.id === state.active);
      node.classList.toggle("loading", tab.loading);
      node.classList.toggle("asleep", tab.asleep);
      node.classList.toggle("audible", tab.audible);

      if (tabsEl.children[index] !== node) {
        tabsEl.insertBefore(node, tabsEl.children[index] || null);
      }
    });

    tabNodes.forEach(function (node, id) {
      if (!seen.has(id)) {
        node.remove();
        tabNodes.delete(id);
      }
    });
  }

  /* -------------------------------------------------------------- omnibox */

  function setUrl(url) {
    state.url = url;
    if (!state.editing) {
      urlEl.value = url === "" || url.indexOf("https://cleandark.assets/") === 0 ? "" : url;
    }
  }

  function openDropdown(items) {
    state.suggestions = items || [];
    state.selected = -1;
    dropdownEl.innerHTML = "";

    if (state.suggestions.length === 0) {
      closeDropdown();
      return;
    }

    state.suggestions.slice(0, 8).forEach(function (item, index) {
      var row = document.createElement("div");
      row.className = "suggestion lift";
      row.innerHTML =
        '<span class="suggestion-title"></span><span class="suggestion-url"></span>';
      row.querySelector(".suggestion-title").textContent = item.title || "";
      row.querySelector(".suggestion-url").textContent = item.url;
      row.addEventListener("pointerdown", function (event) {
        event.preventDefault();
        commit(item.url);
      });
      row.dataset.index = String(index);
      dropdownEl.appendChild(row);
    });

    dropdownEl.classList.add("open");
    // Tell the host how much of the page to overlay. Row height (34) plus a
    // little padding, in CSS pixels — the host scales for DPI.
    var rows = Math.min(state.suggestions.length, 8);
    send({ cmd: "chromeOverlayHeight", px: rows * 34 + 12 });
  }

  function closeDropdown() {
    if (!dropdownEl.classList.contains("open")) {
      return;
    }
    dropdownEl.classList.remove("open");
    dropdownEl.innerHTML = "";
    state.suggestions = [];
    state.selected = -1;
    send({ cmd: "chromeOverlayHeight", px: 0 });
  }

  function moveSelection(delta) {
    var rows = dropdownEl.querySelectorAll(".suggestion");
    if (rows.length === 0) {
      return;
    }
    if (state.selected >= 0 && rows[state.selected]) {
      rows[state.selected].classList.remove("selected");
    }
    state.selected = (state.selected + delta + rows.length + 1) % (rows.length + 1) - 1;
    if (state.selected >= 0 && rows[state.selected]) {
      rows[state.selected].classList.add("selected");
      urlEl.value = state.suggestions[state.selected].url;
    }
  }

  function commit(text) {
    closeDropdown();
    urlEl.blur();
    send({ cmd: "omniboxSubmit", text: text });
  }

  urlEl.addEventListener("focus", function () {
    state.editing = true;
    urlEl.value = state.url.indexOf("https://cleandark.assets/") === 0 ? "" : state.url;
    urlEl.select();
  });

  urlEl.addEventListener("blur", function () {
    state.editing = false;
    setUrl(state.url);
    // Deferred so a pointerdown on a suggestion still lands.
    setTimeout(closeDropdown, 120);
  });

  var queryTimer = 0;
  urlEl.addEventListener("input", function () {
    var text = urlEl.value;
    clearTimeout(queryTimer);
    if (text.trim() === "") {
      closeDropdown();
      return;
    }
    // Debounced: typing fast should not fire a database query per keystroke.
    queryTimer = setTimeout(function () {
      send({ cmd: "queryHistory", query: text, limit: 8 });
    }, 90);
  });

  urlEl.addEventListener("keydown", function (event) {
    if (event.key === "Enter") {
      commit(urlEl.value);
    } else if (event.key === "Escape") {
      closeDropdown();
      urlEl.blur();
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      moveSelection(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      moveSelection(-1);
    }
  });

  /* --------------------------------------------------------------- toolbar */

  backEl.addEventListener("click", function () {
    send({ cmd: "back" });
  });
  forwardEl.addEventListener("click", function () {
    send({ cmd: "forward" });
  });
  el("reload").addEventListener("click", function (event) {
    send({ cmd: "reload", bypassCache: event.shiftKey });
  });
  el("new-tab").addEventListener("click", function () {
    send({ cmd: "newTab" });
  });
  starEl.addEventListener("click", function () {
    send({ cmd: "toggleBookmark" });
  });

  shieldEl.addEventListener("click", function () {
    var host_ = hostOf(state.url);
    if (!host_) {
      return;
    }
    var active = shieldEl.classList.contains("off");
    send({ cmd: "setShieldForHost", host: host_, blocking: active });
  });

  function hostOf(url) {
    try {
      return new URL(url).hostname;
    } catch (e) {
      return "";
    }
  }

  el("win-min").addEventListener("click", function () {
    send({ cmd: "windowMinimize" });
  });
  el("win-max").addEventListener("click", function () {
    send({ cmd: "windowToggleMaximize" });
  });
  el("win-close").addEventListener("click", function () {
    send({ cmd: "windowClose" });
  });

  /* ------------------------------------------------------------------ menu */

  el("menu-btn").addEventListener("click", function (event) {
    event.stopPropagation();
    var open = menuEl.classList.toggle("open");
    // The menu overlays the page, so the host has to grow the chrome for it.
    send({ cmd: "chromeOverlayHeight", px: open ? 300 : 0 });
  });

  document.addEventListener("pointerdown", function (event) {
    if (menuEl.classList.contains("open") && !event.target.closest("#menu, #menu-btn")) {
      menuEl.classList.remove("open");
      send({ cmd: "chromeOverlayHeight", px: 0 });
    }
  });

  var INTERNAL_PAGES = {
    history: "pages/history.html",
    downloads: "pages/downloads.html",
    bookmarks: "pages/bookmarks.html",
    vault: "pages/vault.html"
  };

  menuEl.addEventListener("click", function (event) {
    var item = event.target.closest("[data-action], [data-toggle]");
    if (!item) {
      return;
    }

    var action = item.dataset.action;
    if (action && INTERNAL_PAGES[action]) {
      send({ cmd: "newTab", url: "https://cleandark.assets/" + INTERNAL_PAGES[action] });
    } else if (action === "incognito") {
      send({ cmd: "newIncognitoWindow" });
    }

    var toggle = item.dataset.toggle;
    if (toggle === "adblock") {
      send({ cmd: "setAdblockEnabled", enabled: !state.settings.adblock });
      return;
    }
    if (toggle === "chameleon") {
      send({ cmd: "setChameleon", enabled: !state.settings.chameleon });
      return;
    }
    if (toggle === "reduceMotion") {
      send({ cmd: "setReduceMotion", enabled: !state.settings.reduceMotion });
      return;
    }

    menuEl.classList.remove("open");
    send({ cmd: "chromeOverlayHeight", px: 0 });
  });

  /* ------------------------------------------------------------- shortcuts */

  document.addEventListener("keydown", function (event) {
    var ctrl = event.ctrlKey || event.metaKey;
    if (!ctrl) {
      return;
    }
    var key = event.key.toLowerCase();

    if (key === "t") {
      send({ cmd: "newTab" });
    } else if (key === "w") {
      if (state.active !== null) {
        send({ cmd: "closeTab", id: state.active });
      }
    } else if (key === "l") {
      urlEl.focus();
    } else if (key === "r") {
      send({ cmd: "reload", bypassCache: event.shiftKey });
    } else if (key === "d") {
      send({ cmd: "toggleBookmark" });
    } else if (key === "h") {
      send({ cmd: "newTab", url: "https://cleandark.assets/pages/history.html" });
    } else if (key === "j") {
      send({ cmd: "newTab", url: "https://cleandark.assets/pages/downloads.html" });
    } else if (key === "b") {
      send({ cmd: "newTab", url: "https://cleandark.assets/pages/bookmarks.html" });
    } else {
      return;
    }
    event.preventDefault();
  });

  /* ----------------------------------------------------------------- toast */

  var toastTimer = 0;
  function showToast(message, kind) {
    toastEl.textContent = message;
    toastEl.className = "toast show " + (kind || "info");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () {
      toastEl.className = "toast " + (kind || "info");
    }, 2600);
  }

  /* -------------------------------------------------------- host messages */

  function applySettings(settings) {
    state.settings.adblock = settings.adblockEnabled;
    state.settings.chameleon = settings.chameleonEnabled;
    state.settings.reduceMotion = settings.reduceMotion;

    document.body.classList.toggle("reduce-motion", settings.reduceMotion);
    menuEl
      .querySelector('[data-toggle="adblock"]')
      .classList.toggle("on", settings.adblockEnabled);
    menuEl
      .querySelector('[data-toggle="chameleon"]')
      .classList.toggle("on", settings.chameleonEnabled);
    menuEl
      .querySelector('[data-toggle="reduceMotion"]')
      .classList.toggle("on", settings.reduceMotion);

    if (!settings.chameleonEnabled) {
      document.documentElement.style.removeProperty("--accent");
    }
  }

  if (host) {
    host.addEventListener("message", function (event) {
      var msg = event.data;
      if (!msg || typeof msg.evt !== "string") {
        return;
      }

      switch (msg.evt) {
        case "tabs":
          state.tabs = msg.tabs;
          state.active = msg.active;
          renderTabs();
          break;

        case "navigation":
          if (msg.id !== state.active) {
            break;
          }
          setUrl(msg.url);
          backEl.disabled = !msg.canGoBack;
          forwardEl.disabled = !msg.canGoForward;
          starEl.classList.toggle("on", msg.bookmarked);
          omniboxEl.classList.toggle("secure", msg.secure);
          shieldEl.classList.toggle("off", !msg.shieldActive);
          shieldEl.classList.toggle("has-blocks", msg.blocked > 0);
          shieldCountEl.textContent = msg.blocked > 99 ? "99+" : String(msg.blocked);
          break;

        case "accent":
          // The host validates this is a hex colour before it reaches us; a
          // page cannot inject arbitrary CSS through it.
          if (msg.color) {
            document.documentElement.style.setProperty("--accent", msg.color);
          } else {
            document.documentElement.style.removeProperty("--accent");
          }
          break;

        case "windowMode":
          document.body.classList.toggle("incognito", msg.incognito);
          break;

        case "history":
          // Only relevant while the omnibox is driving the dropdown.
          if (state.editing && urlEl.value.trim() !== "") {
            openDropdown(msg.items);
          }
          break;

        case "settings":
          applySettings(msg.settings);
          break;

        case "toast":
          showToast(msg.message, msg.kind);
          break;

        default:
          break;
      }
    });
  }

  send({ cmd: "querySettings" });
})();
