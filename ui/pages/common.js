/* Helpers shared by the internal pages.
 *
 * Loaded as a plain script (no modules) so the CSP can stay at
 * `script-src 'self'` with no extra origins.
 */

var CD = (function () {
  "use strict";

  var host = window.chrome && window.chrome.webview;

  function send(message) {
    if (host) {
      host.postMessage(message);
    }
  }

  function onEvent(name, handler) {
    if (!host) {
      return;
    }
    host.addEventListener("message", function (event) {
      var msg = event.data;
      if (msg && msg.evt === name) {
        handler(msg);
      }
    });
  }

  function el(id) {
    return document.getElementById(id);
  }

  function hostOf(url) {
    try {
      return new URL(url).hostname.replace(/^www\./, "");
    } catch (e) {
      return url;
    }
  }

  function monogram(text) {
    var match = String(text || "").match(/[a-z0-9]/i);
    return match ? match[0].toUpperCase() : "?";
  }

  /* Relative time, coarse on purpose: history is scanned, not audited. */
  function ago(millis) {
    if (!millis) {
      return "";
    }
    var seconds = Math.max(0, (Date.now() - millis) / 1000);
    if (seconds < 60) {
      return "just now";
    }
    var units = [
      [60, "minute"],
      [3600, "hour"],
      [86400, "day"],
      [604800, "week"]
    ];
    for (var i = units.length - 1; i >= 0; i--) {
      var size = units[i][0];
      if (seconds >= size) {
        var count = Math.floor(seconds / size);
        return count + " " + units[i][1] + (count === 1 ? "" : "s") + " ago";
      }
    }
    return "just now";
  }

  function bytes(value) {
    if (!value || value < 0) {
      return "";
    }
    var units = ["B", "KB", "MB", "GB", "TB"];
    var index = 0;
    var size = value;
    while (size >= 1024 && index < units.length - 1) {
      size /= 1024;
      index++;
    }
    return (index === 0 ? size : size.toFixed(1)) + " " + units[index];
  }

  /* Build a list row. Text is assigned via textContent throughout — nothing
   * from history, a file name or a vault entry is ever parsed as HTML. */
  function row(options) {
    var node = document.createElement("div");
    node.className = "row";

    var mono = document.createElement("div");
    mono.className = "mono";
    mono.textContent = options.monogram;
    node.appendChild(mono);

    var main = document.createElement("div");
    main.className = "row-main";

    var title = document.createElement("div");
    title.className = "row-title";
    title.textContent = options.title;
    main.appendChild(title);

    var sub = document.createElement("div");
    sub.className = "row-sub";
    sub.textContent = options.subtitle;
    main.appendChild(sub);

    node.appendChild(main);

    var actions = document.createElement("div");
    actions.className = "row-actions";
    (options.actions || []).forEach(function (action) {
      var button = document.createElement("button");
      button.className = "icon-btn";
      button.title = action.label;
      button.textContent = action.glyph;
      button.style.fontSize = "13px";
      button.addEventListener("click", function (event) {
        event.stopPropagation();
        action.run();
      });
      actions.appendChild(button);
    });
    node.appendChild(actions);

    if (options.onOpen) {
      node.addEventListener("click", options.onOpen);
    }
    return node;
  }

  function renderList(listEl, emptyEl, items, build) {
    listEl.textContent = "";
    emptyEl.hidden = items.length > 0;
    items.forEach(function (item) {
      listEl.appendChild(build(item));
    });
  }

  return {
    send: send,
    onEvent: onEvent,
    el: el,
    hostOf: hostOf,
    monogram: monogram,
    ago: ago,
    bytes: bytes,
    row: row,
    renderList: renderList
  };
})();
