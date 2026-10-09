//! "Import from cmux…": runs `lmux import-cmux` in the background and shows
//! what it will do, then what it did.
//!
//! The dialog opens on a dry run, so the list of workspaces and anything that
//! will be skipped is on screen before anything changes. Import then runs the
//! real thing (`--move` unless "Leave cmux as it is" is ticked) and lists each
//! workspace's outcome; Show selects the first one that came over.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use egui::{Align2, Key, Vec2};

use crate::theme;
use crate::ui;

#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct Row {
    pub title: String,
    pub host: String,
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub tabs: usize,
    #[serde(default)]
    pub agents: usize,
    #[serde(default)]
    pub agents_resumed: usize,
}

type Slot = Arc<Mutex<Option<Result<Vec<Row>, String>>>>;

#[derive(Clone)]
pub enum Stage {
    /// Just opened: the dry run starts on the first frame drawn.
    Opening,
    Preview(Slot),
    Running(Slot),
    Done(Result<Vec<Row>, String>),
}

#[derive(Clone)]
pub struct CmuxImport {
    pub stage: Stage,
    /// Leave cmux as it is: build copies and start no agent here.
    pub copy: bool,
}

impl CmuxImport {
    pub fn new() -> Self {
        CmuxImport { stage: Stage::Opening, copy: false }
    }
}

impl Default for CmuxImport {
    fn default() -> Self {
        Self::new()
    }
}

/// Run `lmux --json import-cmux <args>` on a thread.
fn start(args: &[&str], ctx: &egui::Context) -> Slot {
    let slot: Slot = Arc::new(Mutex::new(None));
    let out = slot.clone();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = (|| {
            let bin = crate::net::local_server_bin().ok_or("no lmux binary next to the app")?;
            let o = Command::new(bin)
                .arg("--json")
                .arg("import-cmux")
                .args(&args)
                .env("PATH", format!("{}:/opt/homebrew/bin:/usr/local/bin", crate::net::daemon_path()))
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("could not run lmux: {e}"))?;
            if !o.status.success() {
                return Err(String::from_utf8_lossy(&o.stderr).trim().trim_start_matches("lmux: ").to_owned());
            }
            #[derive(serde::Deserialize)]
            struct Out {
                workspaces: Vec<Row>,
            }
            serde_json::from_slice::<Out>(&o.stdout).map(|o| o.workspaces).map_err(|e| format!("unexpected output: {e}"))
        })();
        *out.lock().unwrap() = Some(result);
        ctx.request_repaint();
    });
    slot
}

fn taken(slot: &Slot) -> Option<Result<Vec<Row>, String>> {
    slot.lock().unwrap().clone()
}

fn small(t: &str) -> egui::RichText {
    egui::RichText::new(t).font(theme::ui(theme::FS_SMALL)).color(theme::DIM)
}

fn rows(ui: &mut egui::Ui, rows: &[Row]) {
    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
        egui::Grid::new("cmux-import-rows").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            for r in rows {
                let (word, color) = match r.status.as_str() {
                    "moved" => ("Moved", theme::GREEN),
                    "copied" => ("Copied", theme::GREEN),
                    "planned" => ("Ready", theme::TEXT),
                    _ => ("Skipped", theme::AMBER),
                };
                ui.label(egui::RichText::new(word).font(theme::ui(theme::FS_SMALL)).color(color));
                ui.vertical(|ui| {
                    let on = if r.host == "this mac" { String::new() } else { format!("  ·  {}", r.host) };
                    ui.label(egui::RichText::new(format!("{}{on}", r.title)).font(theme::ui(theme::FS_BODY)));
                    let mut detail = format!("{} tab{}", r.tabs, if r.tabs == 1 { "" } else { "s" });
                    if r.agents > 0 {
                        detail += &match r.status.as_str() {
                            "moved" => format!(", {} of {} Claude resumed", r.agents_resumed, r.agents),
                            _ => format!(", {} Claude", r.agents),
                        };
                    }
                    if r.status != "skipped" {
                        ui.label(small(&detail));
                    }
                    if let Some(why) = &r.reason {
                        ui.label(small(why));
                    }
                });
                ui.end_row();
            }
        });
    });
}

/// Draw the dialog while it is open.
pub fn dialog(ctx: &egui::Context, c: &mut ui::Ctx) {
    let Some(mut imp) = c.state.cmux_import.clone() else { return };
    let mut close = false;
    let mut show: Option<String> = None;
    // A finished job moves the dialog on.
    match &imp.stage {
        Stage::Opening => imp.stage = Stage::Preview(start(&["--dry-run", "--move"], ctx)),
        Stage::Preview(_) => {}
        Stage::Running(slot) => {
            if let Some(r) = taken(slot) {
                imp.stage = Stage::Done(r);
            }
        }
        Stage::Done(_) => {}
    }
    egui::Window::new("Import from cmux")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(440.0, 0.0))
        .show(ctx, |ui| match &imp.stage {
            Stage::Opening => {}
            Stage::Preview(slot) => {
                ui.label(small(
                    "Brings every cmux workspace here with its folder, splits, tabs, directories and browser pages. \
                     Each one closes in cmux, and its Claude resumes here with the same conversation. \
                     A Claude that is working stays in cmux until it is idle.",
                ));
                ui.add_space(6.0);
                let preview = taken(slot);
                match &preview {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(small("Reading cmux…"));
                        });
                    }
                    Some(Err(e)) => {
                        ui.label(egui::RichText::new(e).font(theme::ui(theme::FS_SMALL)).color(theme::RED));
                    }
                    Some(Ok(list)) => rows(ui, list),
                }
                ui.add_space(6.0);
                ui.checkbox(&mut imp.copy, "Leave cmux as it is (copy; Claude sessions are not started here)");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let ready = matches!(&preview, Some(Ok(l)) if l.iter().any(|r| r.status == "planned"));
                    if ui.add_enabled(ready, egui::Button::new("Import")).clicked() {
                        let args: &[&str] = if imp.copy { &[] } else { &["--move"] };
                        imp.stage = Stage::Running(start(args, ctx));
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
            }
            Stage::Running(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(small(if imp.copy {
                        "Building the workspaces in lmux…"
                    } else {
                        "Moving workspaces and handing over Claude sessions…"
                    }));
                });
            }
            Stage::Done(result) => {
                match result {
                    Err(e) => {
                        ui.label(egui::RichText::new(e).font(theme::ui(theme::FS_SMALL)).color(theme::RED));
                    }
                    Ok(list) => {
                        let n = |s: &str| list.iter().filter(|r| r.status == s).count();
                        let skipped = n("skipped");
                        ui.label(egui::RichText::new(format!(
                            "{} workspace{} came over{}",
                            n("moved") + n("copied"),
                            if n("moved") + n("copied") == 1 { "" } else { "s" },
                            if skipped > 0 { format!(", {skipped} skipped") } else { String::new() }
                        )));
                        ui.add_space(4.0);
                        rows(ui, list);
                    }
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let first = result.as_ref().ok().and_then(|l| {
                        l.iter()
                            .find(|r| r.host == "this mac" && matches!(r.status.as_str(), "moved" | "copied"))
                            .and_then(|r| r.workspace.clone())
                    });
                    if let Some(ws) = first {
                        if ui.button("Show").clicked() {
                            show = Some(ws);
                        }
                    }
                    if ui.button("Done").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            }
        });
    if let Some(ws) = show {
        if c.net.is_local_host() {
            c.state.select_workspace(&ws);
        }
        close = true;
    }
    c.state.cmux_import = if close { None } else { Some(imp) };
}
