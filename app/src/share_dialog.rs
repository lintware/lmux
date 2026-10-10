//! "Show QR…": runs `lmux --json share`, which starts a Cloudflare Quick
//! Tunnel and serves the sessions to a browser, and shows the link and QR code.
//!
//! The child's stdin is a pipe the app holds. Stop sharing, or the app quitting
//! (even by a crash), closes it and the child exits with its tunnel.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use egui::{Align2, Color32, Key, Sense, Vec2};

use crate::theme;
use crate::ui;

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Info {
    pub url: String,
    pub typing: bool,
    pub expires_in: u64,
    qr: Qr,
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

fn stop() {
    if let Some(mut run) = RUN.lock().unwrap().take() {
        let _ = run.child.kill();
        let _ = run.child.wait();
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
    egui::Window::new("Share to a phone")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(380.0, 0.0))
        .show(ctx, |ui| {
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
