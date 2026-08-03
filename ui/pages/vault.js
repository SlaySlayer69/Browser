/* Password vault page.
 *
 * The master password is typed here and sent straight to the host, which
 * derives the key with Argon2id. Nothing in this page ever holds a decrypted
 * secret except for the moment a reveal is displayed.
 */

(function () {
  "use strict";

  var gateEl = CD.el("gate");
  var unlockedEl = CD.el("unlocked");
  var listEl = CD.el("list");
  var emptyEl = CD.el("empty");
  var countEl = CD.el("count");

  CD.el("lock").addEventListener("click", function () {
    CD.send({ cmd: "vaultLock" });
  });

  CD.el("add").addEventListener("click", function () {
    var host = window.prompt("Website (host)");
    if (!host) {
      return;
    }
    var username = window.prompt("Username");
    if (username === null) {
      return;
    }
    var password = window.prompt("Password");
    if (password === null) {
      return;
    }
    CD.send({
      cmd: "vaultUpsert",
      host: host,
      username: username,
      password: password,
      note: ""
    });
  });

  /* The gate is the only thing shown while the vault is locked or missing. */
  function renderGate(state) {
    gateEl.textContent = "";
    unlockedEl.hidden = state === "unlocked";

    if (state === "unlocked") {
      gateEl.hidden = true;
      return;
    }
    gateEl.hidden = false;

    var creating = state === "uninitialised";

    var card = document.createElement("div");
    card.className = "card";

    var heading = document.createElement("h2");
    heading.textContent = creating ? "Create your vault" : "Vault locked";
    card.appendChild(heading);

    var blurb = document.createElement("p");
    blurb.textContent = creating
      ? "Pick a master password. It is the only way in — it is never stored, " +
        "and it cannot be recovered if you forget it."
      : "Enter your master password to unlock saved logins.";
    card.appendChild(blurb);

    var field = document.createElement("label");
    field.className = "field";
    var input = document.createElement("input");
    input.type = "password";
    input.autocomplete = "off";
    input.placeholder = "Master password";
    field.appendChild(input);
    card.appendChild(field);

    var confirmInput = null;
    if (creating) {
      var confirmField = document.createElement("label");
      confirmField.className = "field";
      confirmInput = document.createElement("input");
      confirmInput.type = "password";
      confirmInput.autocomplete = "off";
      confirmInput.placeholder = "Repeat master password";
      confirmField.appendChild(confirmInput);
      card.appendChild(confirmField);
    }

    var error = document.createElement("p");
    error.className = "sub";
    error.style.color = "var(--danger)";
    error.style.minHeight = "16px";
    error.style.margin = "6px 0 10px";
    card.appendChild(error);

    var button = document.createElement("button");
    button.className = "btn accent";
    button.textContent = creating ? "Create vault" : "Unlock";
    card.appendChild(button);

    function submit() {
      var value = input.value;
      if (creating) {
        // Twelve characters is not a policy so much as a floor: this key is
        // only as strong as what is typed here.
        if (value.length < 12) {
          error.textContent = "Use at least 12 characters.";
          return;
        }
        if (value !== confirmInput.value) {
          error.textContent = "The two passwords do not match.";
          return;
        }
        CD.send({ cmd: "vaultCreate", masterPassword: value });
      } else {
        CD.send({ cmd: "vaultUnlock", masterPassword: value });
      }
      input.value = "";
      if (confirmInput) {
        confirmInput.value = "";
      }
    }

    button.addEventListener("click", submit);
    [input, confirmInput].forEach(function (node) {
      if (node) {
        node.addEventListener("keydown", function (event) {
          if (event.key === "Enter") {
            submit();
          }
        });
      }
    });

    gateEl.appendChild(card);
    input.focus();
  }

  function renderEntries(items) {
    countEl.textContent =
      items.length + (items.length === 1 ? " credential" : " credentials");

    CD.renderList(listEl, emptyEl, items, function (entry) {
      var node = CD.row({
        monogram: CD.monogram(entry.host),
        title: entry.host,
        subtitle: entry.username + " · updated " + CD.ago(entry.updatedAt),
        actions: [
          {
            label: "Reveal",
            glyph: "◉",
            run: function () {
              CD.send({ cmd: "vaultReveal", id: entry.id });
            }
          },
          {
            label: "Remove",
            glyph: "×",
            run: function () {
              if (window.confirm("Delete the login for " + entry.host + "?")) {
                CD.send({ cmd: "vaultRemove", id: entry.id });
              }
            }
          }
        ]
      });
      node.dataset.id = String(entry.id);
      return node;
    });
  }

  CD.onEvent("vault", function (msg) {
    renderGate(msg.state);
    if (msg.state === "unlocked") {
      if (msg.items) {
        renderEntries(msg.items);
      } else {
        CD.send({ cmd: "vaultList" });
      }
    }
  });

  CD.onEvent("vaultSecret", function (msg) {
    var row = listEl.querySelector('[data-id="' + msg.id + '"]');
    if (!row) {
      return;
    }
    var sub = row.querySelector(".row-sub");
    var previous = sub.textContent;
    sub.textContent = msg.password;
    sub.classList.add("secret");
    // Put the secret back out of sight without needing another interaction.
    setTimeout(function () {
      sub.textContent = previous;
      sub.classList.remove("secret");
    }, 12000);
  });

  CD.send({ cmd: "vaultStatus" });
})();
