/* Downloads page. */

(function () {
  "use strict";

  var listEl = CD.el("list");
  var emptyEl = CD.el("empty");
  var countEl = CD.el("count");

  /* Progress bars are updated in place rather than by re-rendering the list:
   * BytesReceivedChanged fires several times a second per transfer. */
  var fills = new Map();

  CD.el("clear").addEventListener("click", function () {
    if (window.confirm("Clear the download list? Files on disk are kept.")) {
      CD.send({ cmd: "clearDownloads" });
    }
  });

  function percent(received, total) {
    if (!total || total <= 0) {
      return null;
    }
    // Servers sometimes under-report the length; never draw past full.
    return Math.max(0, Math.min(100, (received / total) * 100));
  }

  function subtitle(download) {
    var size =
      download.totalBytes > 0
        ? CD.bytes(download.receivedBytes) + " of " + CD.bytes(download.totalBytes)
        : CD.bytes(download.receivedBytes);

    if (download.state === "inprogress") {
      return size;
    }
    if (download.state === "completed") {
      return CD.bytes(download.totalBytes) + " · " + CD.ago(download.finishedAt);
    }
    return download.state + " · " + CD.ago(download.startedAt);
  }

  function fileName(path) {
    var parts = String(path).split(/[\\/]/);
    return parts[parts.length - 1] || path;
  }

  function render(items) {
    fills.clear();
    countEl.textContent = items.length + (items.length === 1 ? " item" : " items");

    CD.renderList(listEl, emptyEl, items, function (download) {
      var name = fileName(download.targetPath);
      var running = download.state === "inprogress";

      var actions = [
        {
          label: "Show in folder",
          glyph: "⌑",
          run: function () {
            CD.send({ cmd: "showDownloadInFolder", id: download.id });
          }
        },
        {
          label: "Remove from list",
          glyph: "×",
          run: function () {
            CD.send({ cmd: "removeDownload", id: download.id });
          }
        }
      ];

      if (running) {
        actions.unshift({
          label: "Cancel",
          glyph: "■",
          run: function () {
            CD.send({ cmd: "cancelDownload", id: download.id });
          }
        });
      }

      var node = CD.row({
        monogram: CD.monogram(name),
        title: name,
        subtitle: subtitle(download),
        actions: actions,
        onOpen: running
          ? null
          : function () {
              CD.send({ cmd: "openDownload", id: download.id });
            }
      });

      if (running) {
        var bar = document.createElement("div");
        bar.className = "progress";
        var fill = document.createElement("div");
        fill.className = "progress-fill";
        var pct = percent(download.receivedBytes, download.totalBytes);
        fill.style.transform = "scaleX(" + (pct === null ? 0 : pct / 100) + ")";
        bar.appendChild(fill);
        node.querySelector(".row-main").appendChild(bar);
        fills.set(download.id, { fill: fill, sub: node.querySelector(".row-sub") });
      }

      return node;
    });
  }

  CD.onEvent("downloads", function (msg) {
    render(msg.items);
  });

  CD.onEvent("downloadProgress", function (msg) {
    var entry = fills.get(msg.id);
    if (!entry) {
      // A transfer we have not drawn yet; pull the list once.
      CD.send({ cmd: "queryDownloads" });
      return;
    }
    var pct = percent(msg.received, msg.total);
    if (pct !== null) {
      entry.fill.style.transform = "scaleX(" + pct / 100 + ")";
    }
    entry.sub.textContent =
      msg.total > 0
        ? CD.bytes(msg.received) + " of " + CD.bytes(msg.total)
        : CD.bytes(msg.received);
  });

  CD.send({ cmd: "queryDownloads" });
})();
