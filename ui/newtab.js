/* New tab page: search field plus the speed dial grid.
 *
 * This page is served from the trusted asset origin, so the host accepts the
 * full command protocol from it.
 */

(function () {
  "use strict";

  var host = window.chrome && window.chrome.webview;
  var send = function (message) {
    if (host) {
      host.postMessage(message);
    }
  };

  var dialsEl = document.getElementById("dials");
  var hintEl = document.getElementById("hint");
  var queryEl = document.getElementById("q");

  queryEl.addEventListener("keydown", function (event) {
    if (event.key === "Enter" && queryEl.value.trim() !== "") {
      send({ cmd: "omniboxSubmit", text: queryEl.value });
    }
  });

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

  function render(items, suggested) {
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
      add.innerHTML =
        '<div class="dial-mono">+</div><div class="dial-title">Add</div>';
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

  if (host) {
    host.addEventListener("message", function (event) {
      var msg = event.data;
      if (msg && msg.evt === "speedDials") {
        render(msg.items, msg.suggested);
      }
    });
  }

  send({ cmd: "querySpeedDials" });
})();
