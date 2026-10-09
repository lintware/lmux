//! Injected JavaScript for snapshotting, geometry, and DOM mutations.
//!
//! Every snippet is a self-contained expression evaluated with
//! `Runtime.evaluate { returnByValue: true }`.

/// Build the `snapshot -i` accessibility-style tree. `__INTERACTIVE__` is
/// replaced with `true`/`false`. Elements are tagged `data-lmux-ref="eN"` so
/// later commands can resolve `eN` back to a node via a CSS selector.
pub(crate) fn snapshot_expr(interactive: bool) -> String {
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
pub(crate) fn geometry_expr(selector: &str) -> String {
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
pub(crate) fn focus_expr(selector: &str) -> String {
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
pub(crate) fn clear_expr(selector: &str) -> String {
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
pub(crate) fn js_click_expr(selector: &str) -> String {
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
pub(crate) fn select_expr(selector: &str, value: &str) -> String {
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
pub(crate) fn scroll_expr(selector: Option<&str>, dx: f64, dy: f64) -> String {
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
pub(crate) fn get_expr(what: &str, selector: Option<&str>, name: Option<&str>) -> Option<String> {
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
pub(crate) fn link_at_expr(x: f64, y: f64) -> String {
    let (x, y) = (if x.is_finite() { x } else { 0.0 }, if y.is_finite() { y } else { 0.0 });
    format!(
        "(function(){{var el=document.elementFromPoint({x},{y});\
var a=el&&el.closest?el.closest('a[href],area[href]'):null;if(!a)return null;\
var h=a.href;if(h&&typeof h!=='string')h=h.baseVal?new URL(h.baseVal,document.baseURI).href:null;\
if(!h||/^javascript:/i.test(h))return null;return h;}})()"
    )
}

/// JSON-escape a string as a JS string literal (including quotes).
pub(crate) fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}
