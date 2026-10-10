"use strict";
(function () {
  var $ = function (id) { return document.getElementById(id); };
  var ws = null, typing = false, current = null, tree = null;

  function status(msg) {
    var s = $("status");
    s.textContent = msg || "";
    s.hidden = !msg;
  }

  function show(view) {
    $("list").hidden = view !== "list";
    $("term").hidden = view !== "term";
    $("back").hidden = view !== "term";
    if (view !== "term") $("title").textContent = "lmux";
  }

  function setMode() {
    var m = $("mode");
    m.textContent = typing ? "typing on" : "view only";
    m.className = "badge" + (typing ? " on" : "");
    $("composer").hidden = !typing || !current;
  }

  function send(obj) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify(obj));
  }

  function renderTree() {
    var list = $("list");
    list.textContent = "";
    if (!tree.workspaces.length) {
      status("No sessions.");
      return;
    }
    status("");
    tree.workspaces.forEach(function (w) {
      var card = document.createElement("div");
      card.className = "ws";
      var h = document.createElement("h2");
      var dot = document.createElement("span");
      dot.className = "dot " + w.agent;
      h.appendChild(dot);
      h.appendChild(document.createTextNode(w.title));
      card.appendChild(h);
      if (w.summary) {
        var sum = document.createElement("div");
        sum.className = "sum";
        sum.textContent = w.summary;
        card.appendChild(sum);
      }
      w.tabs.forEach(function (t) {
        var b = document.createElement("button");
        b.className = "tab";
        b.textContent = t.title || t.id;
        if (t.kind !== "terminal") b.disabled = true;
        b.addEventListener("click", function () { openTab(t, w); });
        card.appendChild(b);
      });
      list.appendChild(card);
    });
  }

  function openTab(t, w) {
    current = t.id;
    $("title").textContent = w.title + " · " + (t.title || t.id);
    $("screen").textContent = "";
    show("term");
    setMode();
    send({ t: "open", surface: t.id });
  }

  function closeTab() {
    current = null;
    send({ t: "close" });
    show("list");
    setMode();
  }

  function onMessage(ev) {
    var m;
    try { m = JSON.parse(ev.data); } catch (e) { return; }
    if (m.t === "tree") {
      tree = m;
      typing = !!m.typing;
      setMode();
      if (!current) { show("list"); }
      renderTree();
    } else if (m.t === "screen" && m.surface === current) {
      var el = $("screen");
      var atEnd = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
      el.textContent = m.text;
      if (atEnd) el.scrollTop = el.scrollHeight;
    } else if (m.t === "error") {
      status(m.msg);
    }
  }

  function connect() {
    var proto = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(proto + "//" + location.host + "/ws");
    ws.onmessage = onMessage;
    ws.onclose = function () {
      status("Disconnected. The share may have ended or the link expired; scan a new QR code.");
      $("status").hidden = false;
    };
    ws.onerror = function () { /* onclose reports it */ };
  }

  function start() {
    var code = location.hash.length > 1 ? location.hash.slice(1) : "";
    if (!code) { connect(); return; }
    fetch("/auth", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code: code }),
      credentials: "same-origin"
    }).then(function (r) {
      history.replaceState(null, "", location.pathname);
      if (!r.ok) {
        status("This link has expired or was already used. Scan a new QR code.");
        return;
      }
      connect();
    }).catch(function () { status("Could not reach lmux."); });
  }

  $("back").addEventListener("click", closeTab);
  $("composer").addEventListener("submit", function (e) {
    e.preventDefault();
    var input = $("text");
    send({ t: "send", surface: current, text: input.value, enter: true });
    input.value = "";
  });
  Array.prototype.forEach.call(document.querySelectorAll("#keys button"), function (b) {
    b.addEventListener("click", function () { send({ t: "key", surface: current, key: b.getAttribute("data-key") }); });
  });
  start();
})();
