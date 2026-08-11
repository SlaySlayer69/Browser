/* New tab page: clock, search field, speed dials and the privacy hub.
 *
 * This page is served from the trusted asset origin, so the host accepts the
 * full command protocol from it.
 *
 * Everything on a timer here stops when the tab is not visible. A background
 * new-tab page that keeps ticking a clock and polling for statistics is exactly
 * the kind of idle cost this browser exists to avoid.
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

  var dialsEl = el("dials");
  var hintEl = el("hint");
  var queryEl = el("q");

  queryEl.addEventListener("keydown", function (event) {
    if (event.key === "Enter" && queryEl.value.trim() !== "") {
      send({ cmd: "omniboxSubmit", text: queryEl.value });
    }
  });

  /* ------------------------------------------------------------------ clock */

  var timeEl = el("time");
  var dateEl = el("date");

  var DATE_FORMAT = { weekday: "long", day: "numeric", month: "long", year: "numeric" };

  function drawClock() {
    var now = new Date();
    // Locale-driven: a 12-hour locale gets AM/PM, a 24-hour one does not.
    timeEl.textContent = now.toLocaleTimeString(undefined, {
      hour: "2-digit",
      minute: "2-digit"
    });
    dateEl.textContent = now.toLocaleDateString(undefined, DATE_FORMAT);
  }

  /* ----------------------------------------------------------- formatting */

  /* Each formatter returns [value, unit] so the unit can be rendered smaller,
   * the way the big numbers read in a stat card. */

  function compactCount(value) {
    if (value >= 1e9) {
      return [(value / 1e9).toFixed(1), "B"];
    }
    if (value >= 1e6) {
      return [(value / 1e6).toFixed(1), "M"];
    }
    if (value >= 1000) {
      return [(value / 1000).toFixed(1), "K"];
    }
    return [String(value), ""];
  }

  function formatBytes(value) {
    var units = ["B", "KB", "MB", "GB", "TB"];
    var index = 0;
    var size = value;
    while (size >= 1024 && index < units.length - 1) {
      size /= 1024;
      index++;
    }
    // Whole bytes need no decimal; anything scaled reads better with one.
    return [index === 0 ? String(Math.round(size)) : size.toFixed(1), units[index]];
  }

  function formatDuration(millis) {
    var seconds = Math.round(millis / 1000);
    if (seconds < 60) {
      return [String(seconds), "Sec"];
    }
    var minutes = Math.round(seconds / 60);
    if (minutes < 60) {
      return [String(minutes), "Min"];
    }
    var hours = minutes / 60;
    if (hours < 48) {
      return [hours.toFixed(1), "Hrs"];
    }
    return [(hours / 24).toFixed(1), "Days"];
  }

  function setStat(valueId, unitId, pair) {
    el(valueId).textContent = pair[0];
    if (unitId) {
      el(unitId).textContent = pair[1];
    }
  }

  /* --------------------------------------------------------- privacy hub */

  var hubEl = el("hub");

  el("hub-reset").addEventListener("click", function () {
    if (window.confirm("Reset all privacy statistics? Blocking itself is unaffected.")) {
      send({ cmd: "resetPrivacyStats" });
    }
  });

  function renderStats(stats) {
    hubEl.classList.toggle("off", !stats.blockingEnabled);

    setStat("s-blocked", "s-blocked-u", compactCount(stats.blockedTotal));
    setStat("s-bytes", "s-bytes-u", formatBytes(stats.bytesSaved));
    setStat("s-time", "s-time-u", formatDuration(stats.timeSavedMs));
    setStat("s-mem", "s-mem-u", formatBytes(stats.memoryBytes));

    // One decimal below 10%, none above: 3.4% is informative, 41.7% is noise.
    var cpu = stats.cpuPercent;
    el("s-cpu").textContent = cpu < 10 ? cpu.toFixed(1) : String(Math.round(cpu));

    // The sixth tile shows the session count once there is one, since it is
    // the more interesting number; otherwise it falls back to process count.
    if (stats.blockedSession > 0) {
      setStat("s-proc", "s-proc-u", compactCount(stats.blockedSession));
      el("s-proc-label").textContent = "Blocked this session";
    } else {
      setStat("s-proc", "s-proc-u", [String(stats.processCount), ""]);
      el("s-proc-label").textContent = "Browser processes";
    }
  }

  /* ------------------------------------------------------------ speed dials */

  /* A stable colour per host, so a tile keeps the same accent between runs.
   * Derived from the host string rather than stored, and never from page
   * pixels. */
  function accentFor(text) {
    var hash = 0;
    for (var i = 0; i < text.length; i++) {
      hash = (hash * 31 + text.charCodeAt(i)) >>> 0;
    }
    // Fixed saturation and lightness keep every tile legible against the dark
    // surface; only the hue varies.
    return "hsl(" + (hash % 360) + ", 58%, 62%)";
  }

  function hostOf(url) {
    try {
      return new URL(url).hostname.replace(/^www\./, "");
    } catch (e) {
      return url;
    }
  }

  function renderDials(items, suggested) {
    dialsEl.textContent = "";

    items.forEach(function (dial) {
      var tile = document.createElement("div");
      tile.className = "dial";
      tile.title = dial.url;

      var mono = document.createElement("div");
      mono.className = "dial-mono";
      mono.textContent = dial.monogram;
      mono.style.background = dial.accent || accentFor(hostOf(dial.url));

      var title = document.createElement("div");
      title.className = "dial-title";
      title.textContent = dial.title || hostOf(dial.url);

      tile.appendChild(mono);
      tile.appendChild(title);

      // Suggested tiles are not saved yet, so there is nothing to remove.
      if (!suggested) {
        var remove = document.createElement("button");
        remove.className = "icon-btn dial-remove";
        remove.innerHTML =
          '<svg viewBox="0 0 16 16"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/></svg>';
        remove.addEventListener("click", function (event) {
          event.stopPropagation();
          send({ cmd: "removeSpeedDial", id: dial.id });
        });
        tile.appendChild(remove);
      }

      tile.addEventListener("click", function () {
        send({ cmd: "navigate", url: dial.url });
      });

      dialsEl.appendChild(tile);
    });

    // An "add" tile, unless the grid is already full.
    if (items.length < 12) {
      var add = document.createElement("div");
      add.className = "dial add";
      add.innerHTML = '<div class="dial-mono">+</div><div class="dial-title">Add</div>';
      add.addEventListener("click", function () {
        var url = window.prompt("Website address");
        if (!url) {
          return;
        }
        var normalised = /^[a-z]+:\/\//i.test(url) ? url : "https://" + url;
        send({ cmd: "addSpeedDial", url: normalised, title: "", accent: "" });
      });
      dialsEl.appendChild(add);
    }

    hintEl.textContent = suggested
      ? "Suggested from your most visited sites — click Add to pin your own."
      : "";
  }

  /* ------------------------------------------------------------- messages */

  if (host) {
    host.addEventListener("message", function (event) {
      var msg = event.data;
      if (!msg) {
        return;
      }
      if (msg.evt === "speedDials") {
        renderDials(msg.items, msg.suggested);
      } else if (msg.evt === "privacy") {
        renderStats(msg.stats);
      }
    });
  }

  /* ---------------------------------------------------------------- timers */

  /* The stats poll interval is also the CPU averaging window on the host side:
   * CPU time is a counter, so a rate needs two readings spaced apart. */
  var STATS_INTERVAL_MS = 2000;

  var clockTimer = 0;
  var statsTimer = 0;

  function startTimers() {
    if (clockTimer) {
      return;
    }
    drawClock();
    send({ cmd: "queryPrivacyStats" });

    clockTimer = setInterval(drawClock, 1000);
    statsTimer = setInterval(function () {
      send({ cmd: "queryPrivacyStats" });
    }, STATS_INTERVAL_MS);
  }

  function stopTimers() {
    clearInterval(clockTimer);
    clearInterval(statsTimer);
    clockTimer = 0;
    statsTimer = 0;
  }

  document.addEventListener("visibilitychange", function () {
    if (document.hidden) {
      stopTimers();
    } else {
      startTimers();
    }
  });

  if (!document.hidden) {
    startTimers();
  }

  send({ cmd: "querySpeedDials" });
})();
