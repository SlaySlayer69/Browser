/* History page. */

(function () {
  "use strict";

  var listEl = CD.el("list");
  var emptyEl = CD.el("empty");
  var countEl = CD.el("count");
  var searchEl = CD.el("search");

  var timer = 0;
  searchEl.addEventListener("input", function () {
    clearTimeout(timer);
    // Debounced so holding a key does not queue a query per repeat.
    timer = setTimeout(function () {
      CD.send({ cmd: "queryHistory", query: searchEl.value, limit: 300 });
    }, 110);
  });

  CD.el("clear").addEventListener("click", function () {
    if (window.confirm("Delete all browsing history?")) {
      CD.send({ cmd: "clearHistory" });
    }
  });

  CD.onEvent("history", function (msg) {
    countEl.textContent =
      msg.items.length + (msg.items.length === 1 ? " entry" : " entries");

    CD.renderList(listEl, emptyEl, msg.items, function (visit) {
      var title = visit.title || CD.hostOf(visit.url);
      return CD.row({
        monogram: CD.monogram(title),
        title: title,
        subtitle: CD.hostOf(visit.url) + " · " + CD.ago(visit.visitedAt),
        onOpen: function () {
          CD.send({ cmd: "navigate", url: visit.url });
        },
        actions: [
          {
            label: "Remove",
            glyph: "×",
            run: function () {
              CD.send({ cmd: "deleteHistoryEntry", id: visit.id });
            }
          }
        ]
      });
    });
  });

  CD.send({ cmd: "queryHistory", query: "", limit: 300 });
})();
