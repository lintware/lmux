//! End-to-end smoke test for lmux-browser against a local test page.
//!
//! Run on the server (user-data-dir under `~/lmux-dev/browser-state`):
//!
//! ```text
//! source ~/.cargo/env && cd ~/lmux-dev/browser
//! cargo run --release --example smoke
//! ```
//!
//! It exercises: launch, create surface, `snapshot -i`, fill + click by ref,
//! assert the page changed, navigate via a link, go back, collect console/errors,
//! screenshot, then three screencasts (1280x800, 640x400, and one with a slow
//! acker) reporting frames / fps / JPEG size / bandwidth. Finally it navigates to
//! `https://example.com` to prove off-localhost navigation and prints the headless
//! fingerprint it sees.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use lmux_browser::{BrowserConfig, BrowserEvent, BrowserManager};
use lmux_proto::{BrowserInputEvent, Modifiers, MouseAction, MouseButton};

const PAGE1: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>lmux test page</title>
<style>
  body { font-family: sans-serif; margin: 24px; }
  @keyframes slide { from { transform: translateX(0); } to { transform: translateX(320px); } }
  .box { width: 64px; height: 64px; background: linear-gradient(90deg, #e11, #06f); animation: slide 0.9s linear infinite alternate; }
</style></head>
<body>
  <h1>lmux browser smoke</h1>
  <form id="f" onsubmit="event.preventDefault(); submitForm();">
    <label for="name">Name</label>
    <input id="name" type="text" placeholder="your name">
    <button id="go" type="submit">Greet</button>
  </form>
  <div id="out">waiting</div>
  <p><a id="next" href="/page2">Go to page 2</a></p>
  <div class="box"></div>
  <div id="tick">0</div>
  <select id="sel"><option value="a">Alpha</option><option value="b">Beta</option></select>
  <script>
    console.log('page1 ready');
    var c = 0;
    setInterval(function () { c++; document.getElementById('tick').textContent = String(c); }, 100);
    function submitForm() {
      var v = document.getElementById('name').value;
      document.getElementById('out').textContent = 'Hello, ' + v + '!';
      console.log('greeted', v);
    }
  </script>
</body></html>"#;

const PAGE2: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Page Two</title></head>
<body>
  <h1>Page Two</h1>
  <p><a id="back" href="/">Back to page 1</a></p>
  <script>
    console.error('boom from page2');
    throw new Error('page2 deliberate failure');
  </script>
</body></html>"#;

async fn serve(listener: TcpListener) {
    loop {
        let (mut sock, _addr) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let n = match sock.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let body = if path.starts_with("/page2") { PAGE2 } else { PAGE1 };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
    }
}

async fn rpc(mgr: &BrowserManager, surface: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    mgr.handle_rpc(surface, method, params)
        .await
        .map_err(|e| anyhow!("{method} failed: {} ({:?})", e.message, e.code))
}

fn refs_of(snapshot: &Value) -> Vec<Value> {
    snapshot
        .get("refs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn find_ref(refs: &[Value], pred: impl Fn(&Value) -> bool) -> Option<String> {
    refs.iter()
        .find(|r| pred(r))
        .and_then(|r| r.get("ref").and_then(Value::as_str))
        .map(str::to_string)
}

#[derive(Debug, Default)]
struct StreamStats {
    frames: u32,
    bytes: u64,
    sizes: Vec<usize>,
    dims: std::collections::BTreeMap<(u16, u16), u32>,
    /// Frames Chrome produced (encoder work), from `frames_produced`.
    produced: u64,
    elapsed: Duration,
}

impl StreamStats {
    fn fps(&self) -> f64 {
        self.frames as f64 / self.elapsed.as_secs_f64().max(0.001)
    }
    fn report(&self, label: &str) {
        let secs = self.elapsed.as_secs_f64().max(0.001);
        let fps = self.fps();
        let avg = if self.sizes.is_empty() {
            0.0
        } else {
            self.sizes.iter().sum::<usize>() as f64 / self.sizes.len() as f64
        };
        let mut sorted = self.sizes.clone();
        sorted.sort_unstable();
        let p95 = if sorted.is_empty() {
            0.0
        } else {
            let idx = ((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
            sorted[idx.min(sorted.len() - 1)] as f64
        };
        let mbps = (self.bytes as f64 * 8.0) / secs / 1_000_000.0;
        let mut by_freq: Vec<((u16, u16), u32)> = self.dims.iter().map(|(k, v)| (*k, *v)).collect();
        by_freq.sort_by(|a, b| b.1.cmp(&a.1));
        let dims = by_freq
            .iter()
            .take(3)
            .map(|((w, h), _)| format!("{w}x{h}"))
            .collect::<Vec<_>>()
            .join("+");
        let prod_fps = self.produced as f64 / secs;
        println!(
            "  {label:<30} deliv={:<4} fps={:<6.1} avg={:<7.1}KB p95={:<7.1}KB bw={:<6.2}Mbps prod={:<6.1}/s x={:<4.1} dims={:<15} dur={:.1}s",
            self.frames,
            fps,
            avg / 1024.0,
            p95 / 1024.0,
            mbps,
            prod_fps,
            if fps > 0.0 { prod_fps / fps } else { 0.0 },
            if dims.is_empty() { "-".to_string() } else { dims },
            secs,
        );
    }
}

/// Run one screencast phase, acking each frame after `ack_delay`.
async fn screencast_phase(
    mgr: &BrowserManager,
    surface: &str,
    width: u32,
    height: u32,
    dsf: f32,
    duration: Duration,
    ack_delay: Duration,
) -> anyhow::Result<StreamStats> {
    // Subscribe after starting so stale frames from a previous phase are dropped.
    mgr.start_screencast(surface, width, height, dsf).await?;
    let mut rx = mgr.events();
    let produced0 = mgr.frames_produced(surface).await?;
    let started = Instant::now();
    let mut stats = StreamStats::default();
    loop {
        let remaining = duration.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(BrowserEvent::Frame { seq, jpeg, width, height, .. })) => {
                stats.frames += 1;
                stats.bytes += jpeg.len() as u64;
                stats.sizes.push(jpeg.len());
                *stats.dims.entry((width, height)).or_insert(0) += 1;
                if !ack_delay.is_zero() {
                    tokio::time::sleep(ack_delay).await;
                }
                mgr.ack_frame(surface, seq).await?;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_) => break,
        }
    }
    stats.elapsed = started.elapsed();
    stats.produced = mgr.frames_produced(surface).await?.saturating_sub(produced0);
    mgr.stop_screencast(surface).await?;
    Ok(stats)
}

async fn eval_num(mgr: &BrowserManager, surface: &str, js: &str) -> anyhow::Result<f64> {
    let v = rpc(mgr, surface, "browser.eval", json!({"script": js})).await?;
    v["value"]
        .as_f64()
        .ok_or_else(|| anyhow!("eval `{js}` returned non-number: {}", v["value"]))
}

/// Count frames with no screencast running (the no-viewer case).
async fn count_frames(mgr: &BrowserManager, duration: Duration) -> StreamStats {
    let mut rx = mgr.events();
    let started = Instant::now();
    let mut stats = StreamStats::default();
    loop {
        let remaining = duration.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(BrowserEvent::Frame { jpeg, width, height, .. })) => {
                stats.frames += 1;
                stats.bytes += jpeg.len() as u64;
                stats.sizes.push(jpeg.len());
                *stats.dims.entry((width, height)).or_insert(0) += 1;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => continue,
            Err(_) => break,
        }
    }
    stats.elapsed = started.elapsed();
    stats
}

struct CollectedFrame {
    jpeg: Vec<u8>,
    /// The page's proof counter at the moment this frame was delivered.
    tick: u32,
}

/// Stream frames, recording the page's proof counter as each is delivered.
async fn screencast_collect(
    mgr: &BrowserManager,
    surface: &str,
    width: u32,
    height: u32,
    dsf: f32,
    duration: Duration,
    ack_delay: Duration,
) -> anyhow::Result<Vec<CollectedFrame>> {
    // Subscribe after starting so stale frames from a previous phase are dropped.
    mgr.start_screencast(surface, width, height, dsf).await?;
    let mut rx = mgr.events();
    let started = Instant::now();
    let mut out = Vec::new();
    loop {
        let remaining = duration.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(BrowserEvent::Frame { seq, jpeg, .. })) => {
                let tick = eval_num(mgr, surface, "window.__pt").await.unwrap_or(-1.0);
                out.push(CollectedFrame {
                    jpeg,
                    tick: tick.max(0.0) as u32,
                });
                if !ack_delay.is_zero() {
                    tokio::time::sleep(ack_delay).await;
                }
                mgr.ack_frame(surface, seq).await?;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_) => break,
        }
    }
    mgr.stop_screencast(surface).await?;
    Ok(out)
}

/// Decode the centre pixel of a delivered JPEG in-page (reusing Chromium's own
/// decoder via a canvas) and return its RGB.
async fn decode_center_pixel(
    mgr: &BrowserManager,
    surface: &str,
    jpeg: &[u8],
) -> anyhow::Result<[f64; 3]> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
    let lit = serde_json::to_string(&b64)?;
    let expr = format!(
        "(async function(b64){{var img=new Image();img.src='data:image/jpeg;base64,'+b64;\
         await img.decode();var c=document.createElement('canvas');\
         c.width=img.naturalWidth;c.height=img.naturalHeight;\
         var ctx=c.getContext('2d');ctx.drawImage(img,0,0);\
         var d=ctx.getImageData(Math.floor(c.width/2),Math.floor(c.height/2),1,1).data;\
         return [d[0],d[1],d[2]];}})({lit})"
    );
    let v = rpc(mgr, surface, "browser.eval", json!({"script": expr})).await?;
    let arr = v["value"]
        .as_array()
        .ok_or_else(|| anyhow!("pixel decode returned {:?}", v["value"]))?;
    Ok([
        arr.first().and_then(Value::as_f64).unwrap_or(0.0),
        arr.get(1).and_then(Value::as_f64).unwrap_or(0.0),
        arr.get(2).and_then(Value::as_f64).unwrap_or(0.0),
    ])
}

fn rgb_to_hue(r: f64, g: f64, b: f64) -> f64 {
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d == 0.0 {
        return 0.0;
    }
    let h = if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * (((b - r) / d) + 2.0)
    } else {
        60.0 * (((r - g) / d) + 4.0)
    };
    if h < 0.0 {
        h + 360.0
    } else {
        h
    }
}

/// Count Chromium processes whose command line references this profile dir.
fn count_chromium(user_data_dir: &std::path::Path) -> usize {
    let pat = format!("user-data-dir={}", user_data_dir.display());
    match std::process::Command::new("pgrep").args(["-f", &pat]).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        Err(_) => 0,
    }
}

async fn wait_chromium_zero(user_data_dir: &std::path::Path, timeout: Duration) -> usize {
    let start = Instant::now();
    let mut n = count_chromium(user_data_dir);
    while n > 0 && start.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(100)).await;
        n = count_chromium(user_data_dir);
    }
    n
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let surface = "surface:smoke";
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let user_data_dir = std::env::var("LMUX_BROWSER_USER_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(&home).join("lmux-dev/browser-state"));
    let out_dir = std::path::PathBuf::from(&home).join("lmux-dev/browser-state");

    // Local test server.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let base = format!("http://127.0.0.1:{port}/");
    tokio::spawn(serve(listener));
    println!("lmux-browser smoke test");
    println!("  local test page:  {base}");

    let config = BrowserConfig {
        user_data_dir: user_data_dir.clone(),
        window_width: 1280,
        window_height: 800,
        ..BrowserConfig::default()
    };
    println!("  user-data-dir:    {}", user_data_dir.display());
    let t0 = Instant::now();
    let mgr = BrowserManager::launch(config.clone()).await?;
    println!("  chromium launched in {:.0} ms", t0.elapsed().as_millis());

    mgr.create_surface(surface, Some(&base)).await?;
    println!("  surface created");

    // Wait for the page to load.
    rpc(&mgr, surface, "browser.wait", json!({"load_state": "load", "timeout_ms": 10000})).await?;

    let dbg = rpc(
        &mgr,
        surface,
        "browser.eval",
        json!({"script": "location.href + ' | ' + document.readyState + ' | bodylen=' + (document.body ? document.body.innerHTML.length : -1) + ' | title=' + document.title"}),
    )
    .await?;
    println!("  page loaded: {}", dbg["value"]);

    // 1. snapshot -i
    let snap = rpc(&mgr, surface, "browser.snapshot", json!({"interactive": true})).await?;
    let refs = refs_of(&snap);
    println!("\n=== snapshot -i ===\n{}", snap["text"].as_str().unwrap_or(""));
    println!("  refs: {}", refs.len());

    let input_ref = find_ref(&refs, |r| {
        r["tag"] == "input" && r["role"] == "textbox"
    })
    .context("no text input in snapshot")?;
    let button_ref = find_ref(&refs, |r| {
        r["role"] == "button" && r["name"].as_str().unwrap_or("").contains("Greet")
    })
    .context("no greet button in snapshot")?;
    let link_ref = find_ref(&refs, |r| {
        r["tag"] == "a" && r["name"].as_str().unwrap_or("").to_lowercase().contains("page 2")
    })
    .context("no page 2 link in snapshot")?;
    println!("  chosen refs: input={input_ref} button={button_ref} link={link_ref}");

    // 2. fill + click by ref
    rpc(&mgr, surface, "browser.fill", json!({"target": input_ref, "text": "Ada"})).await?;
    rpc(&mgr, surface, "browser.click", json!({"target": button_ref})).await?;

    // 3. assert the page changed via eval
    rpc(&mgr, surface, "browser.wait", json!({"text": "Hello, Ada!", "timeout_ms": 5000})).await?;
    let out = rpc(&mgr, surface, "browser.eval", json!({"script": "document.getElementById('out').textContent"})).await?;
    let out_text = out["value"].as_str().unwrap_or("");
    println!("  page changed: #out = {out_text:?}");
    if !out_text.contains("Hello, Ada!") {
        bail!("fill+click did not update the page: {out_text:?}");
    }

    // 4. select + get
    let sel = rpc(&mgr, surface, "browser.get", json!({"what": "count", "target": "option"})).await?;
    println!("  option count = {}", sel["value"]);

    // 3b. validate BrowserManager::input() forwarding with a real mouse click.
    let _ = rpc(&mgr, surface, "browser.eval", json!({"script": "document.getElementById('out').textContent = 'reset'"})).await?;
    let pt = rpc(
        &mgr,
        surface,
        "browser.eval",
        json!({"script": "(function(){var r=document.getElementById('go').getBoundingClientRect();return [r.left+r.width/2, r.top+r.height/2];})()"}),
    )
    .await?;
    let mx = pt["value"].get(0).and_then(Value::as_f64).unwrap_or(0.0);
    let my = pt["value"].get(1).and_then(Value::as_f64).unwrap_or(0.0);
    let mods = Modifiers::default();
    mgr.input(surface, BrowserInputEvent::Mouse { action: MouseAction::Down, x: mx, y: my, button: MouseButton::Left, click_count: 1, modifiers: mods }).await?;
    mgr.input(surface, BrowserInputEvent::Mouse { action: MouseAction::Up, x: mx, y: my, button: MouseButton::Left, click_count: 1, modifiers: mods }).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let out2 = rpc(&mgr, surface, "browser.eval", json!({"script": "document.getElementById('out').textContent"})).await?;
    let out2 = out2["value"].as_str().unwrap_or("");
    println!("  input() mouse click at ({mx:.0},{my:.0}) -> #out = {out2:?}");
    if !out2.contains("Hello,") {
        bail!("input() mouse click did not activate the button: {out2:?}");
    }

    // 5. navigate via the link
    rpc(&mgr, surface, "browser.click", json!({"target": link_ref})).await?;
    rpc(&mgr, surface, "browser.wait", json!({"url_contains": "/page2", "timeout_ms": 5000})).await?;
    let url = rpc(&mgr, surface, "browser.url", json!({})).await?;
    println!("  navigated to: {} ({:?})", url["url"], url["title"]);

    // 6. go back
    rpc(&mgr, surface, "browser.back", json!({})).await?;
    let back_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let u = rpc(&mgr, surface, "browser.url", json!({})).await?;
        let s = u["url"].as_str().unwrap_or("");
        if s.ends_with(&format!(":{port}/")) {
            println!("  went back to: {s}");
            break;
        }
        if Instant::now() > back_deadline {
            bail!("back did not return to root, at {s}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 7. console + errors
    let console = rpc(&mgr, surface, "browser.console", json!({})).await?;
    let errors = rpc(&mgr, surface, "browser.errors", json!({})).await?;
    let console_n = console["entries"].as_array().map(|a| a.len()).unwrap_or(0);
    let errors_n = errors["entries"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("\n=== console/errors ===");
    println!("  console entries: {console_n}");
    for e in console["entries"].as_array().into_iter().flatten().take(6) {
        println!("    [{}] {}", e["level"].as_str().unwrap_or(""), e["text"].as_str().unwrap_or(""));
    }
    println!("  error entries:   {errors_n}");
    for e in errors["entries"].as_array().into_iter().flatten().take(6) {
        println!("    {}", e["text"].as_str().unwrap_or(""));
    }

    // 8. screenshot to file
    std::fs::create_dir_all(&out_dir)?;
    let shot = out_dir.join("smoke.png");
    let shot_res = rpc(&mgr, surface, "browser.screenshot", json!({"path": shot.to_string_lossy()})).await?;
    let sz = std::fs::metadata(&shot).map(|m| m.len()).unwrap_or(0);
    println!("\n=== screenshot ===\n  {} ({} bytes) -> {}", shot_res["path"], sz, shot.display());

    // 9. screencast: ack-latency sweep on the animated page.
    // Chrome is acked locally on the server, so the page keeps animating at every
    // latency; the client just receives fewer, always-current frames.
    println!("\n=== screencast (animated page, local-ack + drop-to-newest) ===");
    let mut sweep: Vec<(u64, StreamStats)> = Vec::new();
    for delay_ms in [0u64, 5, 10, 20, 33, 60, 200] {
        let dur = if delay_ms >= 200 {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(4)
        };
        let st = screencast_phase(
            &mgr,
            surface,
            1280,
            800,
            1.0,
            dur,
            Duration::from_millis(delay_ms),
        )
        .await?;
        st.report(&format!("1280x800 dsf1 ack-{delay_ms}ms"));
        sweep.push((delay_ms, st));
    }
    let fps0 = sweep[0].1.fps();
    let frames0 = sweep[0].1.frames;
    let frames200 = sweep.last().unwrap().1.frames;

    let small = screencast_phase(&mgr, surface, 640, 400, 1.0, Duration::from_secs(4), Duration::ZERO).await?;
    small.report("640x400 dsf1 ack-0ms");

    // Proof that the page's animation clock keeps advancing at high ack latency.
    // The old model froze it (begin-frames were withheld); the new model must not.
    println!("\n=== animation-clock proof (200 ms ack) ===");
    let clock_js = "document.querySelector('.box').getAnimations()[0].currentTime";
    let clock0 = eval_num(&mgr, surface, clock_js).await?;
    let slow = screencast_phase(&mgr, surface, 1280, 800, 1.0, Duration::from_secs(5), Duration::from_millis(200)).await?;
    slow.report("animation-clock ack-200ms");
    let clock1 = eval_num(&mgr, surface, clock_js).await?;
    let advance = clock1 - clock0;
    println!("  animation currentTime advanced {advance:.0} ms over ~5000 ms wall time");
    if advance < 3500.0 {
        bail!("animation clock froze at high ack latency: advanced only {advance:.0} ms");
    }

    // Proof that the delivered frame is the newest, not a stale one. A full-viewport
    // overlay encodes a ticking counter in its hue; we decode the centre pixel of
    // each delivered JPEG (in-page, via canvas) and compare with the live counter.
    println!("\n=== newest-frame proof (200 ms ack, hue encodes tick) ===");
    let install = r#" (function(){
        var d = document.createElement('div');
        d.id = 'lmuxproof';
        d.style.cssText = 'position:fixed;left:0;top:0;right:0;bottom:0;z-index:2147483647;background:#000';
        document.body.appendChild(d);
        window.__pt = 0;
        window.__ptimer = setInterval(function(){
            window.__pt++;
            d.style.background = 'hsl(' + ((window.__pt * 3) % 360) + ',100%,50%)';
        }, 50);
        return true;
    })() "#;
    rpc(&mgr, surface, "browser.eval", json!({"script": install})).await?;
    let collected = screencast_collect(&mgr, surface, 1280, 800, 1.0, Duration::from_secs(3), Duration::from_millis(200)).await?;
    let uninstall = "(function(){ if(window.__ptimer) clearInterval(window.__ptimer); var d=document.getElementById('lmuxproof'); if(d) d.remove(); return true; })()";
    rpc(&mgr, surface, "browser.eval", json!({"script": uninstall})).await?;
    let mut max_lag = 0i64;
    let mut prev_decoded: Option<i64> = None;
    let mut increasing = true;
    let mut samples = Vec::new();
    for f in &collected {
        let rgb = decode_center_pixel(&mgr, surface, &f.jpeg).await?;
        let hue = rgb_to_hue(rgb[0], rgb[1], rgb[2]);
        let decoded = (hue / 3.0).round() as i64;
        let lag = f.tick as i64 - decoded;
        if let Some(p) = prev_decoded {
            if decoded <= p {
                increasing = false;
            }
        }
        prev_decoded = Some(decoded);
        max_lag = max_lag.max(lag);
        samples.push((f.tick, decoded, lag));
    }
    println!("  delivered frames: {}", samples.len());
    for (tick, decoded, lag) in samples.iter().take(8) {
        println!("    page tick at delivery={tick:<4} decoded from JPEG={decoded:<4} lag={lag} ticks");
    }
    if collected.is_empty() {
        bail!("newest-frame proof received no frames");
    }
    // ~200 ms lag at 50 ms/tick is ~4 ticks; allow generous slack for JPEG + render.
    if max_lag > 20 {
        bail!("delivered frames are stale: max lag {max_lag} ticks");
    }
    if !increasing {
        bail!("delivered frame ticks did not advance; frames repeat a stale value");
    }
    println!("  newest-frame check OK: max lag {max_lag} ticks, ticks increasing = {increasing}");

    // Recovery: a fast client must get full rate back.
    let recover = screencast_phase(&mgr, surface, 1280, 800, 1.0, Duration::from_secs(4), Duration::ZERO).await?;
    recover.report("1280x800 dsf1 ack-0ms (recover)");

    // No viewer: with the screencast stopped (as the daemon does on unsubscribe),
    // nothing is produced.
    println!("\n=== no viewer (screencast stopped) ===");
    // Let any already-in-flight frame drain before we start counting.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let no_viewer = count_frames(&mgr, Duration::from_secs(3)).await;
    no_viewer.report("no-viewer 3s");

    // 10. off-localhost navigation + headless fingerprint
    println!("\n=== network navigation ===");
    rpc(&mgr, surface, "browser.goto", json!({"url": "https://example.com"})).await?;
    let nav_ok = rpc(&mgr, surface, "browser.wait", json!({"load_state": "load", "timeout_ms": 20000}))
        .await
        .is_ok();
    let net = rpc(&mgr, surface, "browser.url", json!({})).await?;
    println!("  reached https://example.com: {nav_ok} -> {} ({:?})", net["url"], net["title"]);
    let fp = rpc(
        &mgr,
        surface,
        "browser.eval",
        json!({"script": "JSON.stringify({ua: navigator.userAgent, webdriver: navigator.webdriver, headless: /Headless/.test(navigator.userAgent), plugins: navigator.plugins.length, languages: navigator.languages, viewport: [innerWidth, innerHeight]})"}),
    )
    .await?;
    println!("  headless fingerprint: {}", fp["value"]);

    // 11. idle bandwidth: a mostly-static page should barely stream.
    println!("\n=== screencast (static page) ===");
    let idle = screencast_phase(&mgr, surface, 1280, 800, 1.0, Duration::from_secs(6), Duration::ZERO).await?;
    idle.report("1280x800 dsf1 static fast-ack");

    // 12. crash restart: SIGKILL Chromium; the supervisor must relaunch and rebuild.
    println!("\n=== crash restart ===");
    let before = mgr.restart_count();
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-f", "user-data-dir=.*browser-state"])
        .status();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut post = None;
    while Instant::now() < deadline {
        if mgr.restart_count() > before {
            // A successful eval needs a live session, so this only passes once the
            // surface has actually been rebuilt.
            if let Ok(t) = rpc(&mgr, surface, "browser.eval", json!({"script": "document.title"})).await {
                post = Some(t);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let post = post.ok_or_else(|| anyhow!("chromium did not restart/rebuild within 30s"))?;
    let u = rpc(&mgr, surface, "browser.url", json!({})).await?;
    println!(
        "  chromium SIGKILLed; restarts={}; surface rebuilt at {} ({:?}); document.title={}",
        mgr.restart_count(),
        u["url"],
        u["title"],
        post["value"]
    );

    // 13. shutdown kills the whole Chromium tree; idempotent; a second launch on the
    // same profile works (this is what would otherwise break on the profile lock).
    println!("\n=== shutdown / relaunch cycle ===");
    // Mirror the daemon: a screencast is actively running when shutdown happens.
    let live = screencast_phase(&mgr, surface, 1280, 800, 1.0, Duration::from_secs(1), Duration::ZERO).await?;
    println!("  active screencast: {} frames delivered, then shutting down", live.frames);
    let before = count_chromium(&user_data_dir);
    println!("  chromium procs (this user-data-dir) before shutdown = {before}");
    mgr.shutdown().await;
    mgr.shutdown().await; // idempotent
    let after = wait_chromium_zero(&user_data_dir, Duration::from_secs(5)).await;
    println!("  chromium procs after shutdown (x2) = {after}");
    if before == 0 {
        bail!("expected at least one chromium process before shutdown");
    }
    if after != 0 {
        bail!("shutdown left {after} chromium processes");
    }

    let mgr2 = BrowserManager::launch(config.clone()).await?;
    mgr2.create_surface(surface, Some(&base)).await?;
    rpc(&mgr2, surface, "browser.wait", json!({"load_state": "load", "timeout_ms": 10000})).await?;
    let title2 = rpc(&mgr2, surface, "browser.eval", json!({"script": "document.title"})).await?;
    let shot2 = out_dir.join("smoke-relaunch.png");
    rpc(&mgr2, surface, "browser.screenshot", json!({"path": shot2.to_string_lossy()})).await?;
    let sz2 = std::fs::metadata(&shot2).map(|m| m.len()).unwrap_or(0);
    println!("  relaunch on same profile OK: title={:?}, screenshot {} bytes", title2["value"], sz2);
    if sz2 == 0 {
        bail!("relaunch screenshot was empty");
    }
    mgr2.shutdown().await;
    let after2 = wait_chromium_zero(&user_data_dir, Duration::from_secs(5)).await;
    println!("  chromium procs after second shutdown = {after2}");
    if after2 != 0 {
        bail!("second shutdown left {after2} chromium processes");
    }

    // Summary
    println!("\n=== summary ===");
    for (d, st) in &sweep {
        st.report(&format!("1280x800 dsf1 ack-{d}ms"));
    }
    small.report("640x400 dsf1 ack-0ms");
    recover.report("1280x800 dsf1 ack-0ms (recover)");
    no_viewer.report("no-viewer 3s");
    idle.report("1280x800 dsf1 static ack-0ms");
    if frames200 >= frames0 {
        bail!("drop-to-newest not observed: 0ms={frames0} frames, 200ms={frames200} frames");
    }
    if recover.fps() < fps0 * 0.8 {
        bail!(
            "stream did not recover after slow acks: recover fps={:.1} vs fast fps={:.1}",
            recover.fps(),
            fps0
        );
    }
    if no_viewer.frames != 0 {
        bail!("no-viewer case produced {} frames", no_viewer.frames);
    }
    let prod_fps = |st: &StreamStats| st.produced as f64 / st.elapsed.as_secs_f64().max(0.001);
    let p0 = prod_fps(&sweep[0].1);
    let plast = &sweep.last().unwrap().1;
    let p_last = prod_fps(plast);
    println!(
        "  drop-to-newest: {frames0} frames at 0ms vs {frames200} at 200ms; recovered to {:.1} fps; \
         animation advanced {advance:.0}ms at 200ms ack; newest-frame max lag {max_lag} ticks; \
         no-viewer frames = {}",
        recover.fps(),
        no_viewer.frames
    );
    println!(
        "  encoder (CPU proxy): Chrome produced {p0:.1} frames/s at 0ms ack vs {p_last:.1}/s at 200ms \
         (client drew {:.1}/s); delivered fps unchanged",
        plast.fps()
    );

    mgr.shutdown().await;
    println!("\nsmoke test OK");
    Ok(())
}
