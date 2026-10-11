"use strict";
(function () {
  var $ = function (id) { return document.getElementById(id); };
  var MAX_TEXT_BYTES = 4096;
  var MAX_RETRIES = 5;
  var LABEL = { needs_input: "Needs input", running: "Working", idle: "Idle", none: "No agent" };
  var ORDER = { needs_input: 0, running: 1, idle: 2, none: 3 };
  var AGENT = { claude: "Claude", codex: "Codex", grok: "Grok", shell: "Shell", other: "Other" };
  var NEAR_END = 48;

  var ws = null, retries = 0, expired = false;
  var typing = false, tree = null, treeKey = "";
  var openId = null, openTab = null, openWs = null, pushed = false;
  var toastTimer = null;
  // View state for the open tab: "chat" or "term", and whether the viewer picked it.
  var mode = "term", picked = false, keysOpen = false;
  // What the chat list shows: one key per rendered message node.
  var chatKeys = [], chatAgent = null, chatLoaded = false;
  var chatStuck = true, screenStuck = true;

  // Theme is the only thing kept in localStorage. Codes and tokens never are.
  function storedTheme() {
    try {
      var v = localStorage.getItem("lmux-theme");
      return v === "light" || v === "dark" ? v : null;
    } catch (e) { return null; }
  }
  function saveTheme(v) {
    try { localStorage.setItem("lmux-theme", v); } catch (e) { /* private window: ignore */ }
  }
  function applyTheme(v) {
    if (v) document.documentElement.setAttribute("data-theme", v);
    else document.documentElement.removeAttribute("data-theme");
    var dark = v ? v === "dark" : window.matchMedia("(prefers-color-scheme: dark)").matches;
    $("theme").textContent = dark ? "Light" : "Dark";
  }

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  }

  function notice(msg) {
    var n = $("notice");
    n.textContent = msg || "";
    n.hidden = !msg;
  }

  function toast(msg) {
    var t = $("toast");
    t.textContent = msg;
    t.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { t.hidden = true; }, 4000);
  }

  function agentOf(w) { return LABEL[w.agent] ? w.agent : "none"; }
  function openable(t) { return t.kind === "terminal"; }
  function agentName(a) { return AGENT[a] || null; }
  function nearEnd(n) { return n.scrollHeight - n.scrollTop - n.clientHeight < NEAR_END; }

  function send(obj) {
    if (!ws || ws.readyState !== 1) return false;
    ws.send(JSON.stringify(obj));
    return true;
  }

  // ---- Markdown, built with createElement and textContent only ----

  var INLINE = "`([^`\\n]+)`|\\*\\*(\\S(?:[^*\\n]|\\*(?!\\*))*?)\\*\\*|__(\\S[^_\\n]*?)__|\\[([^\\]\\n]+)\\]\\((https?:\\/\\/[^)\\s]+)\\)";

  function inline(parent, text) {
    var re = new RegExp(INLINE, "g"), last = 0, m;
    while ((m = re.exec(text))) {
      if (m.index > last) parent.appendChild(document.createTextNode(text.slice(last, m.index)));
      if (m[1] != null) {
        parent.appendChild(el("code", null, m[1]));
      } else if (m[2] != null || m[3] != null) {
        var b = el("strong");
        inline(b, m[2] != null ? m[2] : m[3]);
        parent.appendChild(b);
      } else {
        var a = el("a", null, m[4]);
        a.href = m[5];
        a.target = "_blank";
        a.rel = "noopener noreferrer";
        parent.appendChild(a);
      }
      last = re.lastIndex;
    }
    if (last < text.length) parent.appendChild(document.createTextNode(text.slice(last)));
  }

  function codeBlock(lang, body) {
    var box = el("div", "md-code");
    if (lang) box.appendChild(el("span", "md-lang", lang));
    var pre = el("pre");
    pre.tabIndex = 0;
    pre.appendChild(el("code", null, body));
    box.appendChild(pre);
    return box;
  }

  function markdown(text) {
    var frag = document.createDocumentFragment();
    var lines = text.replace(/\r\n?/g, "\n").split("\n");
    var para = [], quote = [], stack = [], blank = false;

    function flushPara() {
      if (!para.length) return;
      var p = el("p");
      inline(p, para.join("\n"));
      frag.appendChild(p);
      para = [];
    }
    function flushQuote() {
      if (!quote.length) return;
      var q = el("blockquote");
      inline(q, quote.join("\n"));
      frag.appendChild(q);
      quote = [];
    }
    function flush() { flushPara(); flushQuote(); stack = []; }
    function item(type, indent, start, content) {
      flushPara(); flushQuote();
      while (stack.length) {
        var t = stack[stack.length - 1];
        if (indent < t.indent || (indent <= t.indent + 1 && t.type !== type)) stack.pop();
        else break;
      }
      var top = stack[stack.length - 1];
      if (!top || indent > top.indent + 1) {
        var list = el(type);
        if (type === "ol" && start !== 1) list.start = start;
        if (top) top.last.appendChild(list); else frag.appendChild(list);
        top = { type: type, indent: indent, el: list, last: null };
        stack.push(top);
      }
      var li = el("li");
      inline(li, content);
      top.el.appendChild(li);
      top.last = li;
    }

    for (var i = 0; i < lines.length; i++) {
      var line = lines[i], m;
      if ((m = /^(\s*)(`{3,}|~{3,})\s*([^\s`]*)[^`]*$/.exec(line))) {
        flush();
        var close = new RegExp("^\\s*" + (m[2][0] === "`" ? "`" : "~") + "{" + m[2].length + ",}\\s*$");
        var body = [];
        for (i++; i < lines.length && !close.test(lines[i]); i++) body.push(lines[i]);
        frag.appendChild(codeBlock(m[3], body.join("\n")));
        blank = false;
        continue;
      }
      if (/^\s*$/.test(line)) { flushPara(); flushQuote(); blank = true; continue; }
      if (/^\s*\|.*\|\s*$/.test(line)) {
        flush();
        var rows = [line];
        while (i + 1 < lines.length && /^\s*\|.*\|\s*$/.test(lines[i + 1])) rows.push(lines[++i]);
        var t = el("pre", "md-table", rows.join("\n"));
        t.tabIndex = 0;
        frag.appendChild(t);
        blank = false;
        continue;
      }
      if ((m = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line))) {
        flush();
        var h = el("p", "md-h");
        inline(h, m[2]);
        frag.appendChild(h);
        blank = false;
        continue;
      }
      if (/^\s{0,3}([-*_])(\s*\1){2,}\s*$/.test(line)) { flush(); frag.appendChild(el("hr")); blank = false; continue; }
      if ((m = /^(\s*)[-*+•]\s+(.*)$/.exec(line))) { item("ul", m[1].length, 1, m[2]); blank = false; continue; }
      if ((m = /^(\s*)(\d{1,9})[.)]\s+(.*)$/.exec(line))) { item("ol", m[1].length, parseInt(m[2], 10), m[3]); blank = false; continue; }
      if ((m = /^\s*>\s?(.*)$/.exec(line))) { flushPara(); stack = []; quote.push(m[1]); blank = false; continue; }
      if (stack.length && (!blank || /^\s{2,}/.test(line))) {
        // A continuation of the last list item.
        var li = stack[stack.length - 1].last;
        li.appendChild(document.createTextNode(blank ? "\n\n" : "\n"));
        inline(li, line.trim());
        blank = false;
        continue;
      }
      flushQuote();
      stack = [];
      para.push(line);
      blank = false;
    }
    flushPara(); flushQuote();
    return frag;
  }

  // ---- Chat view ----

  function clock(ts) {
    if (!ts) return "";
    var d = new Date(ts);
    if (isNaN(d.getTime())) return "";
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }

  function msgKey(m) { return m.role + "\u0000" + (m.ts || "") + "\u0000" + m.text; }

  function msgNode(m) {
    var text = String(m.text || "");
    if (m.role === "user") {
      var u = el("div", "msg user");
      u.appendChild(el("div", "bubble", text));
      var c = clock(m.ts);
      if (c) u.appendChild(el("span", "msg-time", c));
      return u;
    }
    if (m.role === "tool") {
      var b = el("button", "msg tool", text);
      b.type = "button";
      b.setAttribute("aria-expanded", "false");
      b.addEventListener("click", function () {
        var open = b.classList.toggle("open");
        b.setAttribute("aria-expanded", open ? "true" : "false");
      });
      return b;
    }
    var a = el("div", "msg assistant");
    a.appendChild(markdown(text));
    return a;
  }

  function resetChat() {
    $("chat-list").textContent = "";
    chatKeys = [];
    chatAgent = null;
    chatLoaded = false;
    chatStuck = true;
    $("latest").hidden = true;
    $("chat-empty").textContent = "Loading the conversation…";
    $("chat-empty").hidden = false;
  }

  function scrollChatEnd() {
    var c = $("chat");
    c.scrollTop = c.scrollHeight;
    $("latest").hidden = true;
  }

  // Applies a snapshot. Unchanged messages keep their nodes, so a new message
  // only appends and the oldest falling off the 200 window only removes.
  function renderChat(m) {
    var msgs = Array.isArray(m.messages) ? m.messages : [];
    var keys = msgs.map(msgKey);
    chatAgent = m.agent || chatAgent;
    chatLoaded = true;
    $("chat-empty").textContent = "No messages yet.";
    $("chat-empty").hidden = msgs.length > 0;
    if (keys.length === chatKeys.length && keys.every(function (k, i) { return k === chatKeys[i]; })) {
      updatePlaceholder();
      return;
    }
    var list = $("chat-list"), box = $("chat"), old = chatKeys;
    // The longest run where old[drop..] matches the start of the new snapshot.
    var drop = old.length, same = 0;
    for (var d = 0; d < old.length; d++) {
      var c = 0;
      while (d + c < old.length && c < keys.length && old[d + c] === keys[c]) c++;
      if (c > same) { same = c; drop = d; }
      if (c > 0 && d + c === old.length) break;
    }
    var visible = !$("chatwrap").hidden;
    // Messages after the shared run changed (an edited last message, say).
    for (var r = old.length - 1; r >= drop + same; r--) list.removeChild(list.children[r]);
    // Messages that fell out of the window: keep the reader's place if scrolled up.
    var before = box.scrollHeight, top = box.scrollTop;
    for (var k = 0; k < drop; k++) list.removeChild(list.firstChild);
    if (visible && !chatStuck && drop) box.scrollTop = Math.max(0, top - (before - box.scrollHeight));
    var frag = document.createDocumentFragment();
    for (var j = same; j < msgs.length; j++) frag.appendChild(msgNode(msgs[j]));
    list.appendChild(frag);
    chatKeys = keys;
    if (chatStuck) {
      if (visible) scrollChatEnd();
    } else if (msgs.length > same) {
      $("latest").hidden = false;
    }
    updatePlaceholder();
  }

  // ---- Session view ----

  function findOpen() {
    var found = null;
    if (!tree) return null;
    tree.workspaces.forEach(function (w) {
      w.tabs.forEach(function (t) {
        if (t.id === openId && openable(t)) found = { tab: t, ws: w };
      });
    });
    return found;
  }

  function chatAgentName() {
    return agentName(chatAgent) || agentName(openTab && openTab.agent) || "the agent";
  }

  function updatePlaceholder() {
    $("text").placeholder = mode === "chat" ? "Reply to " + chatAgentName() : "Type to the terminal";
    $("text").setAttribute("aria-label", mode === "chat" ? "Reply" : "Text to type");
  }

  function updateComposer() {
    var on = !!openId;
    $("composer").hidden = !typing || !on;
    $("viewonly").hidden = typing || !on;
    var chat = mode === "chat";
    $("keys-btn").hidden = !chat;
    $("keys").hidden = chat && !keysOpen;
    $("keys-btn").setAttribute("aria-expanded", !$("keys").hidden ? "true" : "false");
    updatePlaceholder();
  }

  function updateWaiting() {
    var waiting = mode === "chat" && openWs && openWs.agent === "needs_input";
    $("waiting").hidden = !waiting;
  }

  function setView(next, byUser) {
    var chatOk = !!(openTab && openTab.chat);
    if (next === "chat" && !chatOk) next = "term";
    if (byUser) picked = true;
    var changed = next !== mode;
    mode = next;
    $("view").setAttribute("data-mode", mode);
    $("chatwrap").hidden = mode !== "chat";
    $("screen").hidden = mode !== "term";
    $("seg").hidden = !chatOk;
    $("seg-chat").setAttribute("aria-selected", mode === "chat" ? "true" : "false");
    $("seg-term").setAttribute("aria-selected", mode === "term" ? "true" : "false");
    if (changed) keysOpen = false;
    updateComposer();
    updateWaiting();
    if (changed) {
      if (mode === "chat" && chatStuck) scrollChatEnd();
      if (mode === "term" && screenStuck) { var s = $("screen"); s.scrollTop = s.scrollHeight; }
    }
  }

  function renderHeader() {
    var t = openTab, w = openWs;
    $("vtitle").textContent = t.title || t.id;
    var sub = [w.title];
    if (agentName(t.agent)) sub.push(agentName(t.agent));
    if (w.git_branch) sub.push(w.git_branch);
    $("vsub").textContent = sub.join(" · ");
    var st = agentOf(w), p = $("vstate");
    p.hidden = st === "none";
    p.textContent = LABEL[st];
    p.className = "pill st-" + st;
  }

  // Keeps the open view in step with the tree. Returns false if the tab is gone.
  function refreshOpen() {
    var f = findOpen();
    if (!f) return false;
    var hadChat = !!(openTab && openTab.chat);
    openTab = f.tab;
    openWs = f.ws;
    renderHeader();
    if (openTab.chat && !hadChat && !picked) setView("chat");
    else setView(mode);
    return true;
  }

  function openView(t, w) {
    if (openId === t.id) return;
    openId = t.id;
    openTab = t;
    openWs = w;
    picked = false;
    keysOpen = false;
    screenStuck = true;
    $("screen").textContent = "";
    resetChat();
    renderHeader();
    $("app").setAttribute("data-view", "view");
    document.body.setAttribute("data-view", "view");
    if (!pushed) {
      // Lets the browser back gesture return to the session list on a phone.
      history.pushState({ lmuxView: true }, "", location.pathname);
      pushed = true;
    }
    mode = "";
    setView(t.chat ? "chat" : "term");
    markActive();
    send({ t: "open", surface: t.id });
  }

  function closeView() {
    if (!openId) return;
    var wasPushed = pushed;
    pushed = false;
    openId = null;
    openTab = null;
    openWs = null;
    send({ t: "close" });
    $("app").setAttribute("data-view", "list");
    document.body.setAttribute("data-view", "list");
    resetChat();
    updateComposer();
    markActive();
    if (wasPushed) history.back();
  }

  window.addEventListener("popstate", function () {
    // The browser already moved back, so the entry we pushed is gone.
    pushed = false;
    if (openId) closeView();
  });

  // ---- Session list ----

  function renderAttention() {
    var n = tree.workspaces.filter(function (w) { return w.agent === "needs_input"; }).length;
    var p = $("attention");
    p.hidden = n === 0;
    p.textContent = n + " need" + (n === 1 ? "s" : "") + " input";
  }

  function markActive() {
    Array.prototype.forEach.call(document.querySelectorAll("#list .tab"), function (b) {
      var on = b.getAttribute("data-id") === openId;
      b.classList.toggle("active", on);
      if (on) b.setAttribute("aria-current", "true"); else b.removeAttribute("aria-current");
    });
  }

  function renderTree() {
    var list = $("list");
    list.textContent = "";
    if (!tree.workspaces.length) {
      list.appendChild(el("p", "empty-list", "No sessions are open right now."));
      return;
    }
    var sorted = tree.workspaces.slice().sort(function (a, b) {
      return (ORDER[agentOf(a)] - ORDER[agentOf(b)]);
    });
    sorted.forEach(function (w) {
      var st = agentOf(w);
      var card = el("section", "card" + (st === "needs_input" ? " attn" : ""));
      var head = el("div", "card-head");
      head.appendChild(el("span", "dot " + st));
      head.appendChild(el("h2", "card-title", w.title));
      head.appendChild(el("span", "pill st-" + st, LABEL[st]));
      card.appendChild(head);
      if (w.summary) card.appendChild(el("p", "sum", w.summary));
      if (w.git_branch) card.appendChild(el("p", "branch", w.git_branch));
      w.tabs.forEach(function (t) {
        var b = el("button", "tab");
        b.type = "button";
        b.setAttribute("data-id", t.id);
        b.appendChild(el("span", "tab-name", t.title || t.id));
        if (openable(t)) {
          var name = agentName(t.agent);
          if (name) b.appendChild(el("span", "agent a-" + t.agent, name));
          b.addEventListener("click", function () { openView(t, w); });
        } else {
          b.disabled = true;
          b.appendChild(el("span", "tab-note", "Not shared"));
        }
        card.appendChild(b);
      });
      list.appendChild(card);
    });
    markActive();
  }

  function onMessage(ev) {
    var m;
    try { m = JSON.parse(ev.data); } catch (e) { return; }
    if (m.t === "tree") {
      tree = m;
      typing = !!m.typing;
      $("host").textContent = m.host || "";
      var key = JSON.stringify(m);
      if (key !== treeKey) {
        treeKey = key;
        renderTree();
      }
      renderAttention();
      if (openId && !refreshOpen()) {
        toast("That tab has closed.");
        closeView();
      }
      updateComposer();
    } else if (m.t === "screen") {
      if (m.surface !== openId) return;
      var s = $("screen");
      var atEnd = s.hidden ? screenStuck : nearEnd(s);
      s.textContent = m.text;
      if (atEnd && !s.hidden) s.scrollTop = s.scrollHeight;
    } else if (m.t === "chat") {
      if (m.surface !== openId) return;
      renderChat(m);
    } else if (m.t === "error") {
      if (/expired/i.test(m.msg || "")) {
        expired = true;
        notice("The session expired. Scan a new QR code to reconnect.");
      } else {
        toast(m.msg || "Something went wrong.");
      }
    }
  }

  function connect() {
    var proto = location.protocol === "https:" ? "wss:" : "ws:";
    var sock = new WebSocket(proto + "//" + location.host + "/ws");
    ws = sock;
    sock.onopen = function () {
      retries = 0;
      notice("");
      // A new socket knows nothing about the tab this page had open.
      if (openId) send({ t: "open", surface: openId });
    };
    sock.onmessage = onMessage;
    sock.onclose = function () {
      if (ws === sock) ws = null;
      if (expired) return;
      if (retries >= MAX_RETRIES) {
        notice("Disconnected. The share may have ended or the link expired. Scan a new QR code.");
        return;
      }
      var wait = Math.min(1000 * Math.pow(2, retries), 8000);
      retries++;
      notice("Reconnecting…");
      setTimeout(connect, wait);
    };
  }

  function clearFragment() {
    history.replaceState(null, "", location.pathname);
  }

  function start() {
    var code = location.hash.length > 1 ? location.hash.slice(1) : "";
    if (!code) { connect(); return; }
    notice("Connecting…");
    fetch("/auth", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code: code }),
      credentials: "same-origin"
    }).then(function (r) {
      clearFragment();
      if (r.ok) { connect(); return; }
      if (r.status === 403) notice("This page was opened from another site, so it was blocked.");
      else notice("This link has expired or was already used. Scan a new QR code.");
    }).catch(function () {
      clearFragment();
      notice("Could not reach lmux. Check the connection and try again.");
    });
  }

  function sendKey(key) {
    if (openId && !send({ t: "key", surface: openId, key: key })) toast("Not connected.");
  }

  $("back").addEventListener("click", function () { closeView(); });
  $("theme").addEventListener("click", function () {
    var dark = document.documentElement.getAttribute("data-theme")
      ? document.documentElement.getAttribute("data-theme") === "dark"
      : window.matchMedia("(prefers-color-scheme: dark)").matches;
    var next = dark ? "light" : "dark";
    saveTheme(next);
    applyTheme(next);
  });
  $("seg-chat").addEventListener("click", function () { setView("chat", true); });
  $("seg-term").addEventListener("click", function () { setView("term", true); });
  $("to-term").addEventListener("click", function () { setView("term", true); });
  $("keys-btn").addEventListener("click", function () {
    keysOpen = !keysOpen;
    updateComposer();
  });
  $("latest").addEventListener("click", function () { chatStuck = true; scrollChatEnd(); });
  $("chat").addEventListener("scroll", function () {
    if ($("chatwrap").hidden) return;
    chatStuck = nearEnd($("chat"));
    if (chatStuck) $("latest").hidden = true;
  }, { passive: true });
  $("screen").addEventListener("scroll", function () {
    if (!$("screen").hidden) screenStuck = nearEnd($("screen"));
  }, { passive: true });
  $("composer").addEventListener("submit", function (e) {
    e.preventDefault();
    if (!openId) return;
    var input = $("text");
    var text = input.value;
    if (new TextEncoder().encode(text).length > MAX_TEXT_BYTES) {
      toast("That is too long to type at once.");
      return;
    }
    if (text === "") {
      sendKey("enter");
    } else if (send({ t: "send", surface: openId, text: text, enter: true })) {
      input.value = "";
      if (mode === "chat") { chatStuck = true; scrollChatEnd(); }
    } else {
      toast("Not connected.");
    }
  });
  Array.prototype.forEach.call(document.querySelectorAll("#keys button"), function (b) {
    b.addEventListener("click", function () { sendKey(b.getAttribute("data-key")); });
  });

  applyTheme(storedTheme());
  start();
})();
