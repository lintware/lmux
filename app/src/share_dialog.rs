//! "Show QR…" and the sidebar's REMOTE button.
//!
//! With a gateway server set (`gateway.json` next to `hosts.json`), the QR
//! comes from the `lmux-gateway` service on that server: the app runs
//! `lmux share-code` there over SSH, so the code works while the laptop is
//! closed. The window names the gateway, counts the code down, notices when
//! it is used, and makes a new one on request.
//!
//! Without a gateway it runs `lmux --json share` on this Mac. The child's
//! stdin is a pipe the app holds. Stop sharing, or the app quitting (even by
//! a crash), closes it and the child exits with its tunnel.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, Key, Sense, Vec2};

use crate::theme;
use crate::ui;

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Info {
    pub url: String,
    pub typing: bool,
    pub expires_in: u64,
    qr: Qr,
    /// The machine serving the link (newer `lmux share` only).
    #[serde(default)]
    pub gateway: Option<String>,
    /// Every server the link reaches, the gateway first.
    #[serde(default)]
    pub servers: Vec<String>,
    /// The code itself (gateway service only), for `--status`.
    #[serde(default)]
    code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
struct Qr {
    size: usize,
    rows: Vec<String>,
}

#[derive(Clone)]
enum Phase {
    Starting,
    Live(Info),
    Failed(String),
}

struct Run {
    child: Child,
    _stdin: ChildStdin,
    phase: Arc<Mutex<Phase>>,
    typing: bool,
}

/// The one share in progress. It outlives the dialog: closing the dialog does
/// not stop sharing.
static RUN: Mutex<Option<Run>> = Mutex::new(None);

/// The dialog's own choice before sharing starts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub allow_typing: bool,
    /// The server picked in "Use as gateway".
    pub gateway_pick: Option<String>,
}

fn start(allow_typing: bool, ctx: &egui::Context) {
    let phase = Arc::new(Mutex::new(Phase::Starting));
    let spawned = (|| -> Result<Run, String> {
        let bin = crate::net::local_server_bin().ok_or("no lmux binary next to the app")?;
        let mut cmd = Command::new(bin);
        cmd.arg("--json").arg("share");
        if allow_typing {
            cmd.arg("--allow-typing");
        }
        let mut child = cmd
            .env("PATH", format!("{}:/opt/homebrew/bin:/usr/local/bin", crate::net::daemon_path()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run lmux: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let out = child.stdout.take().ok_or("no stdout")?;
        let err = child.stderr.take().ok_or("no stderr")?;
        let (p, c) = (phase.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut line = String::new();
            let first = BufReader::new(out).read_line(&mut line).unwrap_or(0);
            let next = match (first, serde_json::from_str::<Info>(line.trim())) {
                (n, Ok(info)) if n > 0 => Phase::Live(info),
                _ => {
                    let mut msg = String::new();
                    for l in BufReader::new(err).lines().map_while(Result::ok) {
                        msg = l;
                    }
                    let msg = msg.trim_start_matches("lmux: ").to_owned();
                    Phase::Failed(if msg.is_empty() { "sharing stopped".into() } else { msg })
                }
            };
            let mut g = p.lock().unwrap();
            // A stopped share stays stopped.
            if matches!(*g, Phase::Starting) {
                *g = next;
            }
            c.request_repaint();
        });
        Ok(Run { child, _stdin: stdin, phase: phase.clone(), typing: allow_typing })
    })();
    match spawned {
        Ok(run) => *RUN.lock().unwrap() = Some(run),
        // No child to keep: show the error without a run.
        Err(e) => *FAILED.lock().unwrap() = Some(Phase::Failed(e)),
    }
}

static FAILED: Mutex<Option<Phase>> = Mutex::new(None);

// --- gateway ---------------------------------------------------------------

/// The phone gateway the app uses: a server running the `lmux-gateway`
/// service (deploy/install-gateway.sh).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Gateway {
    pub ssh_target: String,
}

fn gateway_path() -> Option<std::path::PathBuf> {
    crate::net::config_path().map(|p| p.with_file_name("gateway.json"))
}

/// The gateway in use. Empty until [`load_gateway`] runs, so tests never see
/// the real config.
static GATEWAY: Mutex<Option<Gateway>> = Mutex::new(None);

/// Read the saved gateway; the app calls this once at start.
/// `LMUX_GATEWAY` overrides the file.
pub fn load_gateway() {
    let g = match std::env::var("LMUX_GATEWAY") {
        Ok(t) => (!t.is_empty()).then_some(Gateway { ssh_target: t }),
        Err(_) => gateway_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok()),
    };
    *GATEWAY.lock().unwrap() = g;
}

pub fn gateway() -> Option<Gateway> {
    GATEWAY.lock().unwrap().clone()
}

pub fn set_gateway(g: Option<&Gateway>) -> Result<(), String> {
    *GATEWAY.lock().unwrap() = g.cloned();
    let path = gateway_path().ok_or("no config directory")?;
    match g {
        Some(g) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, serde_json::to_string_pretty(g).unwrap()).map_err(|e| e.to_string())
        }
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        },
    }
}

/// Where a gateway code stands.
#[derive(Clone, Debug, PartialEq)]
enum CodeState {
    Waiting,
    Used,
    Expired,
}

#[derive(Clone)]
enum GwPhase {
    Fetching,
    Ready { info: Info, at: Instant, state: CodeState },
    Failed(String),
}

struct GwRun {
    target: String,
    phase: Arc<Mutex<GwPhase>>,
    /// When the code's status was last asked for.
    checked: Instant,
    checking: Arc<Mutex<bool>>,
}

static GW: Mutex<Option<GwRun>> = Mutex::new(None);

/// Run `lmux share-code [args]` on the gateway over SSH. It shares the SSH
/// connection the app already holds to that server, when there is one, and
/// tries again when the network drops a connection attempt (ssh exits 255).
fn gateway_cmd(target: &str, args: &str) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        let out = Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .arg("-o")
            .arg(format!("ControlPath={}", crate::net::control_path(target).display()))
            .args(["-o", "ControlMaster=auto", "-o", "ControlPersist=60s"])
            .arg(target)
            .arg(format!("~/.local/bin/lmux share-code {args}"))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("could not run ssh: {e}"))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().trim_start_matches("lmux: ").to_owned();
        if last.contains("no lmux gateway is running") {
            return Err(format!("The gateway service is not running on {target}."));
        }
        // Only a failed connection is worth another try.
        if out.status.code() != Some(255) {
            break;
        }
    }
    Err(if last.is_empty() {
        format!("Could not reach {target}.")
    } else if last.contains("timed out") || last.contains("Connection refused") || last.contains("No route") {
        format!("Could not reach {target} over the network ({last}). The gateway keeps running there; try again.")
    } else {
        format!("{target}: {last}")
    })
}

/// Ask the gateway for a fresh code in the background.
fn fetch_code(target: &str, ctx: &egui::Context) {
    let phase = Arc::new(Mutex::new(GwPhase::Fetching));
    *GW.lock().unwrap() = Some(GwRun {
        target: target.to_owned(),
        phase: phase.clone(),
        checked: Instant::now(),
        checking: Arc::default(),
    });
    let (target, ctx) = (target.to_owned(), ctx.clone());
    std::thread::spawn(move || {
        let next = match gateway_cmd(&target, "").and_then(|t| serde_json::from_str::<Info>(&t).map_err(|e| e.to_string())) {
            Ok(info) => GwPhase::Ready { info, at: Instant::now(), state: CodeState::Waiting },
            Err(e) => GwPhase::Failed(e),
        };
        *phase.lock().unwrap() = next;
        ctx.request_repaint();
    });
}

/// Every few seconds while the window is open, ask whether the code was used.
fn poll_code(ctx: &egui::Context) {
    let mut gw = GW.lock().unwrap();
    let Some(run) = gw.as_mut() else { return };
    let code = match &*run.phase.lock().unwrap() {
        GwPhase::Ready { info, state: CodeState::Waiting, .. } => info.code.clone(),
        _ => None,
    };
    let Some(code) = code else { return };
    if run.checked.elapsed() < Duration::from_secs(3) || *run.checking.lock().unwrap() {
        return;
    }
    run.checked = Instant::now();
    *run.checking.lock().unwrap() = true;
    let (target, phase, checking, ctx) = (run.target.clone(), run.phase.clone(), run.checking.clone(), ctx.clone());
    std::thread::spawn(move || {
        let live = gateway_cmd(&target, &format!("--status {code}"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["live"].as_bool());
        if let Some(false) = live {
            if let GwPhase::Ready { info, state, .. } = &mut *phase.lock().unwrap() {
                if info.code.as_deref() == Some(code.as_str()) && *state == CodeState::Waiting {
                    *state = CodeState::Used;
                }
            }
        }
        *checking.lock().unwrap() = false;
        ctx.request_repaint();
    });
}

/// The REMOTE dot: a gateway is set, or this Mac is sharing.
pub fn remote_status() -> (bool, String) {
    if let Some(g) = gateway() {
        return (true, format!("Phone gateway: {}. Click to show a QR code", g.ssh_target));
    }
    if is_sharing() {
        return (true, "Sharing to your phone. Click to show the QR code".into());
    }
    (false, "Show a QR code to control lmux from your phone".into())
}

fn gateway_dialog(ui: &mut egui::Ui, target: &str, close: &mut bool) {
    let ctx = ui.ctx().clone();
    let needs_fetch = GW.lock().unwrap().as_ref().is_none_or(|r| r.target != target);
    if needs_fetch {
        fetch_code(target, &ctx);
    }
    poll_code(&ctx);
    let phase = GW.lock().unwrap().as_ref().map(|r| r.phase.lock().unwrap().clone());
    let mut new_code = false;
    match phase {
        None | Some(GwPhase::Fetching) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(small(&format!("Asking {target} for a pairing code…")));
            });
        }
        Some(GwPhase::Failed(e)) => {
            ui.label(egui::RichText::new(&e).font(theme::ui(theme::FS_SMALL)).color(theme::RED));
            ui.add_space(4.0);
            if !e.contains("over the network") {
                ui.label(small(&format!(
                    "Install or restart it with deploy/install-gateway.sh {target}, \
                     or use this Mac instead."
                )));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Try again").clicked() {
                    new_code = true;
                }
                if ui.button("Use this Mac instead").clicked() {
                    let _ = set_gateway(None);
                    *GW.lock().unwrap() = None;
                }
                if ui.button("Close").clicked() {
                    *close = true;
                }
            });
        }
        Some(GwPhase::Ready { info, at, mut state }) => {
            let left = Duration::from_secs(info.expires_in).saturating_sub(at.elapsed());
            if state == CodeState::Waiting && left.is_zero() {
                state = CodeState::Expired;
            }
            let gw_name = info.gateway.clone().unwrap_or_else(|| target.to_owned());
            ui.label(
                egui::RichText::new(format!("Gateway: {target}"))
                    .font(theme::ui(theme::FS_BODY))
                    .color(theme::TEXT_SOFT),
            );
            let reaches: Vec<&str> = info.servers.iter().map(String::as_str).filter(|s| *s != gw_name).collect();
            ui.label(small(&if reaches.is_empty() {
                format!("Serves the sessions on {gw_name}.")
            } else {
                format!("Serves the sessions on {gw_name} and {}.", reaches.join(", "))
            }));
            ui.add_space(6.0);
            match state {
                CodeState::Waiting => {
                    ui.vertical_centered(|ui| draw_qr(ui, &info.qr));
                    ui.add_space(6.0);
                    let secs = left.as_secs();
                    ui.label(small(&format!(
                        "Scan within {}:{:02}. It works once. {}",
                        secs / 60,
                        secs % 60,
                        if info.typing { "Typing is allowed." } else { "View only." }
                    )));
                    ui.add(egui::Label::new(egui::RichText::new(&info.url).font(theme::mono(11.0))).wrap());
                    ctx.request_repaint_after(Duration::from_secs(1));
                }
                CodeState::Used => {
                    ui.label(egui::RichText::new("Paired. This code was used.").font(theme::ui(theme::FS_BODY)).color(theme::GREEN));
                    ui.label(small("Make a new code to pair another phone or browser."));
                }
                CodeState::Expired => {
                    ui.label(egui::RichText::new("This code expired.").font(theme::ui(theme::FS_BODY)).color(theme::AMBER));
                    ui.label(small("Make a new code to pair."));
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("New code").clicked() {
                    new_code = true;
                }
                if state == CodeState::Waiting && ui.button("Copy link").clicked() {
                    ui.ctx().copy_text(info.url.clone());
                }
                if ui.button("Hide").clicked() {
                    *close = true;
                }
            });
        }
    }
    if new_code {
        fetch_code(target, &ctx);
    }
}

fn gateway_setup(ui: &mut egui::Ui, draft: &mut Draft) {
    ui.add_space(8.0);
    ui.separator();
    ui.label(small(
        "To keep your phone working while this Mac is closed, use a server that runs \
         the lmux-gateway service (deploy/install-gateway.sh) as the phone gateway.",
    ));
    let remotes: Vec<String> = crate::net::load_hosts()
        .into_iter()
        .filter(|h| h.kind == "ssh")
        .map(|h| h.ssh_target.unwrap_or(h.name))
        .collect();
    if remotes.is_empty() {
        return;
    }
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("gateway-pick")
            .selected_text(draft.gateway_pick.clone().unwrap_or_else(|| remotes[0].clone()))
            .show_ui(ui, |ui| {
                for r in &remotes {
                    ui.selectable_value(&mut draft.gateway_pick, Some(r.clone()), r);
                }
            });
        if ui.button("Use as gateway").clicked() {
            let t = draft.gateway_pick.clone().unwrap_or_else(|| remotes[0].clone());
            let _ = set_gateway(Some(&Gateway { ssh_target: t }));
            *GW.lock().unwrap() = None;
        }
    });
}


fn stop() {
    if let Some(run) = RUN.lock().unwrap().take() {
        let Run { mut child, _stdin, .. } = run;
        // Closing stdin makes `lmux share` stop its tunnel and exit; kill only
        // if it does not, which would leave cloudflared running.
        drop(_stdin);
        for _ in 0..30 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    FAILED.lock().unwrap().take();
}

pub fn is_sharing() -> bool {
    RUN.lock().unwrap().is_some()
}

fn small(t: &str) -> egui::RichText {
    egui::RichText::new(t).font(theme::ui(theme::FS_SMALL)).color(theme::DIM)
}

fn draw_qr(ui: &mut egui::Ui, qr: &Qr) {
    let px = 5.0;
    let quiet = 2.0;
    let side = (qr.size as f32 + 2.0 * quiet) * px;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Color32::WHITE);
    for (y, row) in qr.rows.iter().enumerate() {
        for (x, ch) in row.chars().enumerate() {
            if ch == '1' {
                let min = rect.min + Vec2::new((x as f32 + quiet) * px, (y as f32 + quiet) * px);
                p.rect_filled(egui::Rect::from_min_size(min, Vec2::splat(px)), 0.0, Color32::BLACK);
            }
        }
    }
}

/// Draw the dialog while it is open (`open` is the state's flag).
pub fn dialog(ctx: &egui::Context, c: &mut ui::Ctx) {
    let Some(mut draft) = c.state.share_draft.clone() else { return };
    let mut close = false;
    let mut begin = false;
    let mut stop_now = false;
    let phase = RUN
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| (r.phase.lock().unwrap().clone(), r.typing))
        .or_else(|| FAILED.lock().unwrap().clone().map(|p| (p, false)));
    let gw = gateway();
    egui::Window::new("Share to a phone")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(380.0, 0.0))
        .show(ctx, |ui| {
            if let Some(g) = &gw {
                gateway_dialog(ui, &g.ssh_target, &mut close);
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
                return;
            }
            match &phase {
                None => {
                    ui.label(small(
                        "Opens your sessions in a phone browser through a Cloudflare Quick Tunnel: \
                         the list, each tab's live screen, and (if you allow it) typing. \
                         Anyone with the link can see them, so it works once and expires in a few minutes.",
                    ));
                    ui.add_space(6.0);
                    ui.checkbox(&mut draft.allow_typing, "Allow typing into terminals (view only otherwise)");
                    ui.add_space(4.0);
                    ui.label(small(
                        "Quick Tunnel limits: the address changes every time, there is no uptime \
                         guarantee, and Cloudflare refuses requests above 200 in flight.",
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Start sharing").clicked() {
                            begin = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                    gateway_setup(ui, &mut draft);
                }
                Some((Phase::Starting, _)) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(small("Starting the tunnel (the first run downloads cloudflared)…"));
                    });
                    ui.add_space(6.0);
                    if ui.button("Cancel").clicked() {
                        stop_now = true;
                    }
                }
                Some((Phase::Live(info), typing)) => {
                    ui.vertical_centered(|ui| draw_qr(ui, &info.qr));
                    ui.add_space(6.0);
                    ui.label(small(if *typing { "Typing is allowed." } else { "View only." }));
                    ui.add(egui::Label::new(egui::RichText::new(&info.url).font(theme::mono(11.0))).wrap());
                    ui.add_space(4.0);
                    ui.label(small(&format!(
                        "Scan it or open the link within {} minutes; it works once. \
                         The address changes every time and there is no uptime guarantee.",
                        info.expires_in / 60
                    )));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Copy link").clicked() {
                            ui.ctx().copy_text(info.url.clone());
                        }
                        if ui.button("Stop sharing").clicked() {
                            stop_now = true;
                        }
                        if ui.button("Hide").clicked() {
                            close = true;
                        }
                    });
                }
                Some((Phase::Failed(e), _)) => {
                    ui.label(egui::RichText::new(e).font(theme::ui(theme::FS_SMALL)).color(theme::RED));
                    ui.add_space(6.0);
                    if ui.button("Close").clicked() {
                        stop_now = true;
                    }
                }
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
        });
    if stop_now {
        stop();
        close = true;
    }
    if begin {
        start(draft.allow_typing, ctx);
    }
    c.state.share_draft = if close { None } else { Some(draft) };
}

/// End any share (the app is quitting).
pub fn shutdown() {
    stop();
}
