/* Bookmarks page. */

(function () {
  "use strict";

  var listEl = CD.el("list");
  var emptyEl = CD.el("empty");
  var countEl = CD.el("count");

  CD.onEvent("bookmarks", function (msg) {
    countEl.textContent =
      msg.items.length + (msg.items.length === 1 ? " bookmark" : " bookmarks");

    CD.renderList(listEl, emptyEl, msg.items, function (bookmark) {
      var title = bookmark.title || CD.hostOf(bookmark.url);
      return CD.row({
        monogram: CD.monogram(title),
        title: title,
        subtitle: bookmark.url,
        onOpen: function () {
          CD.send({ cmd: "navigate", url: bookmark.url });
        },
        actions: [
          {
            label: "Rename",
            glyph: "✎",
            run: function () {
              var next = window.prompt("Bookmark name", title);
              if (next !== null && next.trim() !== "") {
                CD.send({ cmd: "renameBookmark", id: bookmark.id, title: next.trim() });
              }
            }
          },
          {
            label: "Remove",
            glyph: "×",
            run: function () {
              CD.send({ cmd: "removeBookmark", id: bookmark.id });
            }
          }
        ]
      });
    });
  });

  CD.send({ cmd: "queryBookmarks" });
})();
