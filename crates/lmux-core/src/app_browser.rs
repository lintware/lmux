//! Agent control of WebKit browsers. Each `browser.*` command becomes steps
//! ([`AppBrowserOp`]) run in one page by a [`Steps`]: the lmux app, for the
//! browsers it draws (`in_app` surfaces, under `lmux server --app-browser`),
//! or a WebKit helper process on the host (`crate::webkit`). The steps are
//! mostly the same JavaScript the Chromium backend runs
//! (`lmux_proto::browser_js`); input is DOM events, since WebKit has no CDP.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use lmux_proto::browser_js as js;
use lmux_proto::browser_js::target_selector;
use lmux_proto::methods as m;
use lmux_proto::{AppBrowserOp, RpcError, RpcErrorCode};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::daemon::Inner;

/// How long one step may take before the agent is told the app did not answer.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// Steps sent to the app and waiting for its answer, by call id.
#[derive(Default)]
pub struct AppCalls {
    next: u64,
    waiting: HashMap<u64, oneshot::Sender<Result<Value, String>>>,
}

impl AppCalls {
    pub fn start(&mut self) -> (u64, oneshot::Receiver<Result<Value, String>>) {
        self.next += 1;
        let (tx, rx) = oneshot::channel();
        self.waiting.insert(self.next, tx);
        (self.next, rx)
    }

    /// The app's answer to call `id`.
    pub fn finish(&mut self, id: u64, value: Value, error: Option<String>) {
        if let Some(tx) = self.waiting.remove(&id) {
            let _ = tx.send(match error {
                Some(e) => Err(e),
                None => Ok(value),
            });
        }
    }

    pub fn forget(&mut self, id: u64) {
        self.waiting.remove(&id);
    }
}

fn error(code: RpcErrorCode, message: impl Into<String>) -> RpcError {
    RpcError { code, message: message.into() }
}

fn not_found(message: impl Into<String>) -> RpcError {
    error(RpcErrorCode::NotFound, message)
}

fn invalid(message: impl Into<String>) -> RpcError {
    error(RpcErrorCode::InvalidParams, message)
}

fn pstr(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn req(params: &Value, key: &str) -> Result<String, RpcError> {
    pstr(params, key).ok_or_else(|| invalid(format!("missing `{key}`")))
}

/// Runs one step of a command in a WebKit page. An `Eval` answers with
/// WebKit's JSON for the script's value, as text (see [`decode`]); a
/// `Screenshot` with `{"png_base64": ...}`.
#[async_trait]
pub trait Steps: Send + Sync {
    async fn step(&self, surface: &str, op: AppBrowserOp) -> Result<Value, RpcError>;
}

/// Steps run by the lmux app connected to this daemon, for in-app browsers.
pub struct ViaApp<'a>(pub &'a Arc<Inner>);

#[async_trait]
impl Steps for ViaApp<'_> {
    async fn step(&self, surface: &str, op: AppBrowserOp) -> Result<Value, RpcError> {
        step_via_app(self.0, surface, op).await
    }
}

/// Run one step in the app drawing `surface`.
async fn step_via_app(inner: &Arc<Inner>, surface: &str, op: AppBrowserOp) -> Result<Value, RpcError> {
    let (id, rx) = inner
        .call_app(surface, op)
        .await
        .map_err(|e| error(RpcErrorCode::Unsupported, e))?;
    match tokio::time::timeout(STEP_TIMEOUT, rx).await {
        Ok(Ok(Ok(v))) => Ok(v),
        Ok(Ok(Err(e))) => Err(error(RpcErrorCode::Internal, e)),
        Ok(Err(_)) => Err(error(RpcErrorCode::Internal, "the lmux app went away")),
        Err(_) => {
            inner.app_calls.lock().unwrap().forget(id);
            Err(error(RpcErrorCode::Internal, "the lmux app did not answer"))
        }
    }
}

/// What a script wrapped by `browser_js::wrap_expr` came back as: the app
/// hands over WebKit's result (a JSON string holding `{ok, v}` or
/// `{ok: false, e}`), or nothing when the script could not even be parsed.
fn decode(raw: &Value) -> Result<Value, RpcError> {
    let text = raw.as_str().unwrap_or("");
    if text.is_empty() {
        return Err(error(RpcErrorCode::Internal, "the script did not run (syntax error?)"));
    }
    let inner: Value = serde_json::from_str(text).map_err(|e| error(RpcErrorCode::Internal, format!("bad result: {e}")))?;
    let r: Value = match inner.as_str() {
        Some(s) => serde_json::from_str(s).map_err(|e| error(RpcErrorCode::Internal, format!("bad result: {e}")))?,
        None => inner,
    };
    if r.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(r.get("v").cloned().unwrap_or(Value::Null))
    } else {
        let e = r.get("e").and_then(Value::as_str).unwrap_or("script error");
        Err(error(RpcErrorCode::Internal, format!("page: {e}")))
    }
}

/// Evaluate a JS expression in the page and return its value.
async fn eval(s: &dyn Steps, surface: &str, expr: &str) -> Result<Value, RpcError> {
    let raw = s.step(surface, AppBrowserOp::Eval { script: js::wrap_expr(expr) }).await?;
    decode(&raw)
}

/// An element command's `{found: ...}` answer as an error when nothing matched.
fn found(v: &Value, sel: &str) -> Result<(), RpcError> {
    if v.get("found").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(not_found(format!("no element for `{sel}`")))
    }
}

/// `ctrl+shift+a` / `Enter`: modifiers, and the DOM `key` for the rest.
fn parse_key(combo: &str) -> (String, bool, bool, bool, bool) {
    let (mut ctrl, mut alt, mut shift, mut meta) = (false, false, false, false);
    let mut key = String::new();
    let parts: Vec<&str> = combo.split('+').collect();
    for (i, p) in parts.iter().enumerate() {
        let low = p.to_ascii_lowercase();
        if i + 1 < parts.len() {
            match low.as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "option" => alt = true,
                "shift" => shift = true,
                "cmd" | "meta" | "command" | "super" => meta = true,
                _ => {}
            }
            continue;
        }
        key = match low.as_str() {
            "enter" | "return" => "Enter".into(),
            "escape" | "esc" => "Escape".into(),
            "tab" => "Tab".into(),
            "backspace" => "Backspace".into(),
            "delete" | "del" => "Delete".into(),
            "space" => " ".into(),
            "up" | "arrowup" => "ArrowUp".into(),
            "down" | "arrowdown" => "ArrowDown".into(),
            "left" | "arrowleft" => "ArrowLeft".into(),
            "right" | "arrowright" => "ArrowRight".into(),
            "home" => "Home".into(),
            "end" => "End".into(),
            "pageup" => "PageUp".into(),
            "pagedown" => "PageDown".into(),
            _ => p.to_string(),
        };
    }
    (key, ctrl, alt, shift, meta)
}

/// Every `browser.*` method for a WebKit browser, with the results the
/// Chromium backend gives.
pub async fn handle_rpc(s: &dyn Steps, surface: &str, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        m::BROWSER_GOTO => {
            let url = req(params, "url")?;
            s.step(surface, AppBrowserOp::Navigate { url: url.clone() }).await?;
            Ok(json!({ "url": url }))
        }
        m::BROWSER_BACK => s.step(surface, AppBrowserOp::Back).await.map(|_| json!({})),
        m::BROWSER_FORWARD => s.step(surface, AppBrowserOp::Forward).await.map(|_| json!({})),
        m::BROWSER_RELOAD => s.step(surface, AppBrowserOp::Reload).await.map(|_| json!({})),
        m::BROWSER_STOP => s.step(surface, AppBrowserOp::Stop).await.map(|_| json!({})),
        m::BROWSER_LINK_AT => {
            let num = |k: &str| params.get(k).and_then(Value::as_f64).ok_or_else(|| invalid(format!("missing `{k}`")));
            let href = eval(s, surface, &js::link_at_expr(num("x")?, num("y")?)).await?;
            Ok(json!({ "href": href }))
        }
        m::BROWSER_URL => {
            let v = eval(s, surface, "({url: location.href, title: document.title})").await?;
            Ok(json!({ "url": v.get("url").cloned().unwrap_or(Value::Null), "title": v.get("title").cloned().unwrap_or(Value::Null) }))
        }
        m::BROWSER_SNAPSHOT => {
            let interactive = params.get("interactive").and_then(Value::as_bool).unwrap_or(false);
            let v = eval(s, surface, &js::snapshot_expr(interactive)).await?;
            Ok(json!({
                "text": v.get("text").and_then(Value::as_str).unwrap_or(""),
                "refs": v.get("refs").cloned().unwrap_or_else(|| json!([])),
            }))
        }
        m::BROWSER_CLICK | m::BROWSER_DBLCLICK | m::BROWSER_HOVER => {
            let sel = target_selector(&req(params, "target")?);
            let count = match method {
                m::BROWSER_CLICK => 1,
                m::BROWSER_DBLCLICK => 2,
                _ => 0,
            };
            found(&eval(s, surface, &js::pointer_expr(&sel, count)).await?, &sel)?;
            Ok(json!({}))
        }
        m::BROWSER_TYPE => {
            let sel = target_selector(&req(params, "target")?);
            let text = req(params, "text")?;
            found(&eval(s, surface, &js::type_expr(Some(&sel), &text)).await?, &sel)?;
            Ok(json!({ "typed": text.chars().count() }))
        }
        m::BROWSER_FILL => {
            let sel = target_selector(&req(params, "target")?);
            let text = pstr(params, "text").unwrap_or_default();
            found(&eval(s, surface, &js::clear_expr(&sel)).await?, &sel)?;
            eval(s, surface, &js::type_expr(Some(&sel), &text)).await?;
            Ok(json!({ "filled": text.chars().count() }))
        }
        m::BROWSER_PRESS => {
            let combo = req(params, "key")?;
            if let Some(target) = pstr(params, "target") {
                let sel = target_selector(&target);
                found(&eval(s, surface, &js::focus_expr(&sel)).await?, &sel)?;
            }
            let (key, ctrl, alt, shift, meta) = parse_key(&combo);
            if key.is_empty() {
                return Err(invalid(format!("unknown key `{combo}`")));
            }
            eval(s, surface, &js::press_expr(&key, ctrl, alt, shift, meta)).await?;
            Ok(json!({}))
        }
        m::BROWSER_SELECT => {
            let sel = target_selector(&req(params, "target")?);
            let value = req(params, "value")?;
            let r = eval(s, surface, &js::select_expr(&sel, &value)).await?;
            found(&r, &sel)?;
            if r.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(not_found(format!("no option matching `{value}`")));
            }
            Ok(json!({ "value": r.get("value").cloned().unwrap_or(Value::Null) }))
        }
        m::BROWSER_SCROLL => {
            let dx = params.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
            let dy = params.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
            let sel = pstr(params, "target").map(|t| target_selector(&t));
            let r = eval(s, surface, &js::scroll_expr(sel.as_deref(), dx, dy)).await?;
            if r.get("found").and_then(Value::as_bool) == Some(false) {
                return Err(not_found("scroll target not found"));
            }
            Ok(r)
        }
        m::BROWSER_WAIT => {
            let timeout = Duration::from_millis(params.get("timeout_ms").and_then(Value::as_u64).unwrap_or(5000));
            let start = Instant::now();
            loop {
                if wait_condition(s, surface, params).await? {
                    return Ok(json!({ "waited_ms": start.elapsed().as_millis() as u64 }));
                }
                if start.elapsed() >= timeout {
                    return Err(not_found("wait timed out"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        m::BROWSER_EVAL => {
            let script = req(params, "script")?;
            // An expression first; statements (`const x = 1; x`) parse only
            // through `eval`.
            let raw = s.step(surface, AppBrowserOp::Eval { script: js::wrap_expr(&script) }).await?;
            let v = if raw.as_str().is_some_and(|s| !s.is_empty()) {
                decode(&raw)?
            } else {
                let raw = s.step(surface, AppBrowserOp::Eval { script: js::wrap_statements(&script) }).await?;
                decode(&raw)?
            };
            Ok(json!({ "value": v }))
        }
        m::BROWSER_SCREENSHOT => {
            let r = s.step(surface, AppBrowserOp::Screenshot).await?;
            let b64 = r
                .get("png_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| error(RpcErrorCode::Internal, "screenshot returned no data"))?;
            match pstr(params, "path") {
                Some(path) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .map_err(|e| error(RpcErrorCode::Internal, format!("bad screenshot: {e}")))?;
                    tokio::fs::write(&path, bytes)
                        .await
                        .map_err(|e| error(RpcErrorCode::Internal, format!("write {path}: {e}")))?;
                    Ok(json!({ "path": path }))
                }
                None => Ok(json!({ "png_base64": b64 })),
            }
        }
        m::BROWSER_GET => {
            let what = req(params, "what")?;
            let sel = pstr(params, "target").map(|t| target_selector(&t));
            let name = pstr(params, "name");
            let expr = js::get_expr(&what, sel.as_deref(), name.as_deref())
                .ok_or_else(|| invalid(format!("unknown what `{what}`")))?;
            Ok(json!({ "value": eval(s, surface, &expr).await? }))
        }
        m::BROWSER_CONSOLE | m::BROWSER_ERRORS => {
            let clear = params.get("clear").and_then(Value::as_bool).unwrap_or(false);
            let which = if method == m::BROWSER_CONSOLE { "console" } else { "errors" };
            Ok(json!({ "entries": eval(s, surface, &js::log_expr(which, clear)).await? }))
        }
        m::BROWSER_TABS => match pstr(params, "action").as_deref() {
            // One page per lmux tab: another page is another tab
            // (`lmux new-surface --type browser`).
            Some("list") => {
                let v = eval(s, surface, "({url: location.href, title: document.title})").await?;
                Ok(json!({ "tabs": [{ "index": 0, "url": v["url"], "title": v["title"], "active": true }], "active": 0 }))
            }
            _ => Err(error(
                RpcErrorCode::Unsupported,
                "this browser has one page; open another with `lmux new-surface --type browser`",
            )),
        },
        other => Err(error(RpcErrorCode::Unsupported, format!("{other} is not available for browsers in the lmux app"))),
    }
}

async fn wait_condition(s: &dyn Steps, surface: &str, params: &Value) -> Result<bool, RpcError> {
    if let Some(sel) = pstr(params, "selector") {
        let expr = format!("!!document.querySelector({})", js::json_string(&target_selector(&sel)));
        if eval(s, surface, &expr).await?.as_bool() != Some(true) {
            return Ok(false);
        }
    }
    if let Some(text) = pstr(params, "text") {
        let expr = format!("!!document.body && document.body.innerText.includes({})", js::json_string(&text));
        if eval(s, surface, &expr).await?.as_bool() != Some(true) {
            return Ok(false);
        }
    }
    if let Some(frag) = pstr(params, "url_contains") {
        let url = eval(s, surface, "location.href").await?;
        if !url.as_str().unwrap_or("").contains(&frag) {
            return Ok(false);
        }
    }
    if let Some(state) = pstr(params, "load_state") {
        let ready = eval(s, surface, "document.readyState").await?;
        let ready = ready.as_str().unwrap_or("");
        let ok = match state.as_str() {
            "load" | "networkidle" => ready == "complete",
            "domcontentloaded" => ready == "interactive" || ready == "complete",
            _ => true,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_what_webkit_hands_back() {
        // WebKit's JSON for the string the wrapper returns.
        let raw = Value::String(serde_json::to_string(r#"{"ok":true,"v":{"a":1}}"#).unwrap());
        assert_eq!(decode(&raw).unwrap(), json!({"a": 1}));
        let raw = Value::String(serde_json::to_string(r#"{"ok":false,"e":"boom"}"#).unwrap());
        assert_eq!(decode(&raw).unwrap_err().message, "page: boom");
        assert!(decode(&Value::String(String::new())).is_err());
    }

    #[test]
    fn keys_and_modifiers() {
        assert_eq!(parse_key("Enter"), ("Enter".into(), false, false, false, false));
        assert_eq!(parse_key("ctrl+shift+a"), ("a".into(), true, false, true, false));
        assert_eq!(parse_key("cmd+Left"), ("ArrowLeft".into(), false, false, false, true));
        assert_eq!(parse_key("space"), (" ".into(), false, false, false, false));
    }
}
