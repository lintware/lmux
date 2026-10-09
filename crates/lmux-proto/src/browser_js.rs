//! Injected JavaScript for snapshotting, geometry, and DOM mutations, shared
//! by the Chromium backend (`Runtime.evaluate { returnByValue: true }`) and
//! the browsers the app draws with WebKit (`lmux-core`'s `app_browser`).
//!
//! Every snippet is a self-contained expression.

/// `e12`: an element ref from `snapshot`.
pub fn is_ref(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() >= 2 && b[0] == b'e' && b[1..].iter().all(u8::is_ascii_digit)
}

/// A command's target as a CSS selector: a snapshot ref, or a selector as is.
pub fn target_selector(target: &str) -> String {
    if is_ref(target) {
        format!("[data-lmux-ref=\"{target}\"]")
    } else {
        target.to_string()
    }
}

/// Build the `snapshot -i` accessibility-style tree. `__INTERACTIVE__` is
/// replaced with `true`/`false`. Elements are tagged `data-lmux-ref="eN"` so
/// later commands can resolve `eN` back to a node via a CSS selector.
pub fn snapshot_expr(interactive: bool) -> String {
    SNAPSHOT_JS.replace("__INTERACTIVE__", if interactive { "true" } else { "false" })
}

const SNAPSHOT_JS: &str = r#"
(() => {
  var INTERACTIVE = __INTERACTIVE__;
  document.querySelectorAll('[data-lmux-ref]').forEach(function (e) { e.removeAttribute('data-lmux-ref'); });
  var out = { text: '', refs: [] };
  var refs = out.refs;
  var lines = [];
  var n = 0;

  function roleOf(el) {
    var explicit = el.getAttribute('role');
    var t = el.tagName.toLowerCase();
    if (explicit && explicit !== 'presentation' && explicit !== 'none') return explicit;
    if (t === 'a') return el.hasAttribute('href') ? 'link' : 'generic';
    if (t === 'button') return 'button';
    if (t === 'textarea') return 'textbox';
    if (t === 'select') return 'combobox';
    if (t === 'option') return 'option';
    if (t === 'input') {
      var ty = (el.getAttribute('type') || 'text').toLowerCase();
      if (ty === 'checkbox') return 'checkbox';
      if (ty === 'radio') return 'radio';
      if (ty === 'submit' || ty === 'button' || ty === 'reset' || ty === 'image') return 'button';
      if (ty === 'search') return 'searchbox';
      if (ty === 'range') return 'slider';
      if (ty === 'number') return 'spinbutton';
      if (ty === 'hidden') return 'hidden';
      return 'textbox';
    }
    if (/^h[1-6]$/.test(t)) return 'heading';
    if (t === 'img') return 'img';
    if (t === 'form') return 'form';
    if (t === 'nav') return 'navigation';
    if (t === 'main') return 'main';
    if (t === 'header') return 'banner';
    if (t === 'footer') return 'contentinfo';
    if (t === 'label') return 'label';
    if (t === 'dialog') return 'dialog';
    if (t === 'iframe') return 'iframe';
    if (t === 'table') return 'table';
    if (t === 'li') return 'listitem';
    return 'generic';
  }
  var INTERACTIVE_ROLES = { link:1, button:1, checkbox:1, radio:1, textbox:1, searchbox:1, combobox:1, slider:1, spinbutton:1, option:1, menuitem:1, menuitemcheckbox:1, tab:1, switch:1 };
  var STRUCTURAL = { heading:1, img:1, form:1, navigation:1, main:1, banner:1, contentinfo:1, dialog:1, iframe:1 };

  function visible(el) {
    var s = getComputedStyle(el);
    if (s.display === 'none' || s.visibility === 'hidden' || parseFloat(s.opacity || '1') === 0) return false;
    if (el.getAttribute('aria-hidden') === 'true') return false;
    var r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0 && !(el.tagName === 'INPUT' && el.type === 'hidden')) return false;
    return true;
  }
  function nameOf(el) {
    var aria = el.getAttribute('aria-label');
    if (aria) return aria.trim();
    var lb = el.getAttribute('aria-labelledby');
    if (lb) { var lt = document.getElementById(lb); if (lt && lt.innerText) return lt.innerText.trim().replace(/\s+/g, ' '); }
    var t = el.tagName;
    if (t === 'INPUT' || t === 'SELECT' || t === 'TEXTAREA') {
      if (el.labels && el.labels.length) { var ls = (el.labels[0].innerText || '').trim().replace(/\s+/g, ' '); if (ls) return ls; }
      var ph = el.getAttribute('placeholder'); if (ph) return ph;
      if (t === 'SELECT') { var o = el.options[el.selectedIndex]; if (o) return (o.textContent || '').trim(); }
      return '';
    }
    if (t === 'IMG') return (el.getAttribute('alt') || '').trim();
    var txt = (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ');
    if (txt !== '') return txt.length > 140 ? txt.slice(0, 137) + '...' : txt;
    var title = el.getAttribute('title'); if (title) return title.trim();
    return '';
  }
  function walk(el, depth) {
    if (el.nodeType !== 1) return;
    var t = el.tagName.toLowerCase();
    if (t === 'script' || t === 'style' || t === 'noscript' || t === 'template' || t === 'head' || t === 'meta' || t === 'link') return;
    if (!visible(el)) return;
    var role = roleOf(el);
    var interactive = !!INTERACTIVE_ROLES[role] ||
      el.hasAttribute('onclick') || el.hasAttribute('onmousedown') ||
      (el.hasAttribute('tabindex') && el.getAttribute('tabindex') !== '-1');
    var structural = !!STRUCTURAL[role];
    var include = interactive || structural;
    if (!INTERACTIVE) {
      var hasOwnText = false;
      var cn = el.childNodes;
      for (var i = 0; i < cn.length; i++) { if (cn[i].nodeType === 3 && cn[i].textContent.trim() !== '') { hasOwnText = true; break; } }
      include = include || hasOwnText || el.hasAttribute('aria-label');
    }
    var childDepth = depth;
    if (include) {
      var name = nameOf(el);
      if (name !== '' || interactive || structural) {
        n += 1;
        var ref = 'e' + n;
        el.setAttribute('data-lmux-ref', ref);
        refs.push({ ref: ref, role: role, name: name, tag: t });
        lines.push({ depth: depth, ref: ref, role: role, name: name, el: el });
        childDepth = depth + 1;
      }
    }
    var kids = el.children;
    for (var k = 0; k < kids.length; k++) walk(kids[k], childDepth);
  }

  var body = document.body;
  if (body) walk(body, 0);

  function esc(s) { return String(s).replace(/\\/g, '\\\\').replace(/"/g, '\\"'); }
  for (var li = 0; li < lines.length; li++) {
    var L = lines[li];
    var line = '  '.repeat(L.depth) + '- ' + L.role;
    if (L.name) line += ' "' + esc(L.name) + '"';
    var el = L.el;
    var tag = el.tagName;
    if (L.role === 'heading' && tag.length === 2) line += ' [level=' + tag[1] + ']';
    if ((tag === 'INPUT' || tag === 'TEXTAREA') && el.type !== 'password') {
      var v = el.value;
      if (v !== undefined && v !== null && v !== '') line += ' [value="' + esc(v) + '"]';
      var ph2 = el.getAttribute('placeholder');
      if (ph2) line += ' [placeholder="' + esc(ph2) + '"]';
    }
    if (el.type === 'checkbox' || el.type === 'radio') line += el.checked ? ' [checked]' : ' [unchecked]';
    if (el.disabled) line += ' [disabled]';
    if (el.required) line += ' [required]';
    line += ' [ref=' + L.ref + ']';
    out.text += line + '\n';
  }
  return out;
})()
"#;

/// Scroll the element into view and return its viewport-centre point in CSS px.
pub fn geometry_expr(selector: &str) -> String {
    format!(
        r#"
(() => {{
  var el = document.querySelector({sel});
  if (!el) return {{ found: false }};
  try {{ el.scrollIntoView({{ block: 'center', inline: 'center', behavior: 'instant' }}); }} catch (e) {{ el.scrollIntoView(); }}
  var r = el.getBoundingClientRect();
  var s = getComputedStyle(el);
  var vis = r.width > 0 && r.height > 0 && s.display !== 'none' && s.visibility !== 'hidden';
  return {{ found: true, x: r.left + r.width / 2, y: r.top + r.height / 2, w: r.width, h: r.height, visible: vis, tag: el.tagName.toLowerCase() }};
}})()
"#,
        sel = json_string(selector)
    )
}

/// Focus the element (no click).
pub fn focus_expr(selector: &str) -> String {
    format!(
        r#"
(() => {{
  var el = document.querySelector({sel});
  if (!el) return {{ found: false }};
  el.focus();
  return {{ found: true }};
}})()
"#,
        sel = json_string(selector)
    )
}

/// Clear the element's value in a React/Vue-friendly way (native setter + input event).
pub fn clear_expr(selector: &str) -> String {
    format!(
        r#"
(() => {{
  var el = document.querySelector({sel});
  if (!el) return {{ found: false }};
  el.focus();
  try {{
    if (el.isContentEditable) {{
      el.textContent = '';
    }} else if (el.tagName === 'TEXTAREA') {{
      var st = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
      st.call(el, '');
    }} else if (el.tagName === 'INPUT') {{
      var si = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
      si.call(el, '');
    }}
  }} catch (e) {{ el.value = ''; }}
  el.dispatchEvent(new Event('input', {{ bubbles: true }}));
  return {{ found: true }};
}})()
"#,
        sel = json_string(selector)
    )
}

/// Fallback JS click when the element has no usable box.
pub fn js_click_expr(selector: &str) -> String {
    format!(
        r#"
(() => {{
  var el = document.querySelector({sel});
  if (!el) return {{ found: false }};
  el.click();
  return {{ found: true }};
}})()
"#,
        sel = json_string(selector)
    )
}

/// Select an `<option>` by value or visible text, then fire input/change.
pub fn select_expr(selector: &str, value: &str) -> String {
    format!(
        r#"
(() => {{
  var el = document.querySelector({sel});
  if (!el) return {{ found: false }};
  var want = {val};
  var ok = false;
  if (el.tagName === 'SELECT') {{
    for (var i = 0; i < el.options.length; i++) {{
      var o = el.options[i];
      if (o.value === want || (o.textContent || '').trim() === want) {{ el.value = o.value; ok = true; break; }}
    }}
  }} else {{
    el.value = want; ok = true;
  }}
  if (ok) {{
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
  }}
  return {{ found: true, ok: ok, value: el.value }};
}})()
"#,
        sel = json_string(selector),
        val = json_string(value)
    )
}

/// `scrollTo` on the window, or `scrollIntoView` for a target.
pub fn scroll_expr(selector: Option<&str>, dx: f64, dy: f64) -> String {
    match selector {
        Some(sel) => format!(
            r#"
(() => {{
  var el = document.querySelector({sel});
  if (!el) return {{ found: false }};
  el.scrollIntoView({{ block: 'center', inline: 'center' }});
  window.scrollBy({dx}, {dy});
  return {{ found: true, x: window.scrollX, y: window.scrollY }};
}})()
"#,
            sel = json_string(sel),
            dx = dx,
            dy = dy
        ),
        None => format!(
            r#"
(() => {{ window.scrollBy({dx}, {dy}); return {{ found: true, x: window.scrollX, y: window.scrollY }}; }})()
"#,
            dx = dx,
            dy = dy
        ),
    }
}

/// `browser.get` snippets. Returns a JS expression; `null` means "not handled".
pub fn get_expr(what: &str, selector: Option<&str>, name: Option<&str>) -> Option<String> {
    let sel = selector.map(json_string);
    Some(match what {
        "url" => "location.href".to_string(),
        "title" => "document.title".to_string(),
        "text" => {
            let s = sel?;
            format!(
                "(function(){{var el=document.querySelector({s});return el?el.innerText:null;}})()"
            )
        }
        "html" => match sel {
            Some(s) => format!(
                "(function(){{var el=document.querySelector({s});return el?el.outerHTML:null;}})()"
            ),
            None => "document.documentElement.outerHTML".to_string(),
        },
        "value" => {
            let s = sel?;
            format!("(function(){{var el=document.querySelector({s});return el?el.value:null;}})()")
        }
        "attr" => {
            let s = sel?;
            let n = json_string(name.unwrap_or(""));
            format!(
                "(function(){{var el=document.querySelector({s});return el?el.getAttribute({n}):null;}})()"
            )
        }
        "count" => {
            let s = sel?;
            format!("document.querySelectorAll({s}).length")
        }
        _ => return None,
    })
}

/// The absolute URL of the link under the viewport point (`x`, `y`), or null.
/// `javascript:` links yield null: they cannot be opened in another tab.
pub fn link_at_expr(x: f64, y: f64) -> String {
    let (x, y) = (if x.is_finite() { x } else { 0.0 }, if y.is_finite() { y } else { 0.0 });
    format!(
        "(function(){{var el=document.elementFromPoint({x},{y});\
var a=el&&el.closest?el.closest('a[href],area[href]'):null;if(!a)return null;\
var h=a.href;if(h&&typeof h!=='string')h=h.baseVal?new URL(h.baseVal,document.baseURI).href:null;\
if(!h||/^javascript:/i.test(h))return null;return h;}})()"
    )
}

/// JSON-escape a string as a JS string literal (including quotes).
pub fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

// ---------------------------------------------------------------------------
// WebKit (browsers the app draws): no CDP input, so clicks, typing and keys
// are DOM events; results come back as a JSON string.
// ---------------------------------------------------------------------------

/// `expr` evaluated to a JSON string `{ok, v}` or `{ok: false, e}`. WebKit
/// hands results through NSJSONSerialization, which cannot carry every value
/// (and an exception comes back as nothing), so the page serializes it.
pub fn wrap_expr(expr: &str) -> String {
    format!(
        "(function(){{var r;try{{r={{ok:true,v:({expr})}};}}catch(e){{r={{ok:false,e:String(e&&e.message||e)}};}}\
try{{return JSON.stringify(r,function(k,x){{return x===undefined?null:x;}});}}\
catch(e){{return JSON.stringify({{ok:true,v:String(r.v)}});}}}})()"
    )
}

/// An agent's own script, which may be statements: its completion value, as
/// with `Runtime.evaluate`. Uses `eval`, so a page whose CSP forbids it fails
/// here; try [`wrap_expr`] first.
pub fn wrap_statements(script: &str) -> String {
    wrap_expr(&format!("(0,eval)({})", json_string(script)))
}

/// Hover (`count` 0), click (1) or double click (2) `selector` with the
/// pointer and mouse events a real click fires.
pub fn pointer_expr(selector: &str, count: u32) -> String {
    format!(
        r#"
(() => {{
  var el = document.querySelector({sel});
  if (!el) return {{ found: false }};
  try {{ el.scrollIntoView({{ block: 'center', inline: 'center', behavior: 'instant' }}); }} catch (e) {{ el.scrollIntoView(); }}
  var r = el.getBoundingClientRect();
  var x = r.left + r.width / 2, y = r.top + r.height / 2;
  var o = function (n) {{ return {{ bubbles: true, cancelable: true, composed: true, view: window, clientX: x, clientY: y, button: 0, buttons: n, detail: 1 }}; }};
  var fire = function (C, type, n) {{ try {{ el.dispatchEvent(new C(type, o(n))); }} catch (e) {{ el.dispatchEvent(new MouseEvent(type, o(n))); }} }};
  var P = window.PointerEvent || MouseEvent;
  fire(P, 'pointerover', 0); fire(MouseEvent, 'mouseover', 0);
  fire(P, 'pointerenter', 0); fire(MouseEvent, 'mouseenter', 0);
  fire(P, 'pointermove', 0); fire(MouseEvent, 'mousemove', 0);
  for (var i = 1; i <= {count}; i++) {{
    fire(P, 'pointerdown', 1); fire(MouseEvent, 'mousedown', 1);
    if (el.focus) el.focus();
    fire(P, 'pointerup', 0); fire(MouseEvent, 'mouseup', 0);
    el.click();
  }}
  if ({count} === 2) fire(MouseEvent, 'dblclick', 0);
  return {{ found: true }};
}})()
"#,
        sel = json_string(selector),
        count = count
    )
}

/// Type `text` into `selector` (or the focused element) one character at a
/// time, with key events around each, as typing does.
pub fn type_expr(selector: Option<&str>, text: &str) -> String {
    format!(
        r#"
(() => {{
  var sel = {sel};
  var el = sel ? document.querySelector(sel) : (document.activeElement || document.body);
  if (!el) return {{ found: false }};
  if (sel && el.focus) el.focus();
  var t = {text};
  for (var ch of t) {{
    var k = {{ key: ch, bubbles: true, cancelable: true, composed: true }};
    var go = el.dispatchEvent(new KeyboardEvent('keydown', k));
    el.dispatchEvent(new KeyboardEvent('keypress', k));
    if (go) {{
      if (!document.execCommand('insertText', false, ch)) {{
        if ('value' in el && typeof el.setRangeText === 'function') {{
          var s = el.selectionStart == null ? el.value.length : el.selectionStart;
          var e = el.selectionEnd == null ? s : el.selectionEnd;
          el.setRangeText(ch, s, e, 'end');
          el.dispatchEvent(new InputEvent('input', {{ bubbles: true, data: ch, inputType: 'insertText' }}));
        }}
      }}
    }}
    el.dispatchEvent(new KeyboardEvent('keyup', k));
  }}
  return {{ found: true, typed: [...t].length }};
}})()
"#,
        sel = selector.map(json_string).unwrap_or_else(|| "null".into()),
        text = json_string(text)
    )
}

/// One key (DOM `key`, e.g. "Enter", "a") with modifiers, sent to the focused
/// element, then what the browser would do for it unless the page cancelled
/// the keydown: Enter submits a form, Tab moves focus, Backspace deletes.
pub fn press_expr(key: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> String {
    format!(
        r#"
(() => {{
  var el = document.activeElement || document.body;
  var key = {key};
  var k = {{ key: key, bubbles: true, cancelable: true, composed: true, ctrlKey: {ctrl}, altKey: {alt}, shiftKey: {shift}, metaKey: {meta} }};
  var go = el.dispatchEvent(new KeyboardEvent('keydown', k));
  if (key.length === 1) el.dispatchEvent(new KeyboardEvent('keypress', k));
  if (go) {{
    var tag = (el.tagName || '').toLowerCase();
    if (key === 'Enter') {{
      if (tag === 'input' && el.form) {{
        if (el.form.requestSubmit) el.form.requestSubmit(); else el.form.submit();
      }} else if (tag === 'textarea' || el.isContentEditable) {{
        document.execCommand('insertLineBreak');
      }} else if (tag === 'a' || tag === 'button' || el.getAttribute('role') === 'button') {{
        el.click();
      }}
    }} else if (key === ' ' && (tag === 'button' || (tag === 'input' && /^(checkbox|radio|submit|button)$/i.test(el.type)))) {{
      el.click();
    }} else if (key === 'Backspace') {{
      document.execCommand('delete');
    }} else if (key === 'Delete') {{
      document.execCommand('forwardDelete');
    }} else if (key === 'Tab') {{
      var f = Array.prototype.filter.call(document.querySelectorAll('a[href],button,input,select,textarea,[tabindex]'), function (x) {{
        return x.tabIndex >= 0 && !x.disabled && x.offsetParent !== null;
      }});
      var i = f.indexOf(el);
      var n = f[(i + ({shift} ? f.length - 1 : 1)) % f.length];
      if (n) n.focus();
    }} else if (key.length === 1 && !{ctrl} && !{meta}) {{
      document.execCommand('insertText', false, key);
    }}
  }}
  el.dispatchEvent(new KeyboardEvent('keyup', k));
  return {{ found: true }};
}})()
"#,
        key = json_string(key),
        ctrl = ctrl,
        alt = alt,
        shift = shift,
        meta = meta
    )
}

/// Runs at the start of every page the app draws: keeps the console and
/// uncaught errors for `browser console` / `browser errors`.
pub const CAPTURE_INIT: &str = r#"
(function () {
  if (window.__lmuxLog) return;
  var log = window.__lmuxLog = { console: [], errors: [] };
  var keep = function (list, e) { list.push(e); if (list.length > 500) list.shift(); };
  var text = function (args) {
    return Array.prototype.map.call(args, function (a) {
      if (typeof a === 'string') return a;
      try { return JSON.stringify(a); } catch (e) { return String(a); }
    }).join(' ');
  };
  ['log', 'info', 'warn', 'error', 'debug'].forEach(function (m) {
    var orig = console[m];
    console[m] = function () {
      var type = m === 'warn' ? 'warning' : m;
      keep(log.console, { type: type, level: type === 'error' ? 'error' : type === 'warning' ? 'warning' : 'info', text: text(arguments), timestamp: Date.now() });
      return orig.apply(console, arguments);
    };
  });
  window.addEventListener('error', function (e) {
    keep(log.errors, { type: 'exception', level: 'error', text: String(e.message || e), url: e.filename || null, line: e.lineno || null, column: e.colno || null });
  });
  window.addEventListener('unhandledrejection', function (e) {
    keep(log.errors, { type: 'exception', level: 'error', text: 'Unhandled rejection: ' + String(e.reason && e.reason.message || e.reason), url: null, line: null, column: null });
  });
})();
"#;

/// The console (`which` "console") or uncaught errors ("errors") kept by
/// [`CAPTURE_INIT`], emptied after reading when `clear`.
pub fn log_expr(which: &str, clear: bool) -> String {
    format!(
        "(function(){{var l=window.__lmuxLog;if(!l)return [];var r=l[{w}].slice();if({clear})l[{w}].length=0;return r;}})()",
        w = json_string(which),
        clear = clear
    )
}
