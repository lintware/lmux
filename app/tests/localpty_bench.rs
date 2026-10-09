//! Benchmark: keystroke to echo through the local PTY path.
//!
//! Two measurements:
//!
//! 1. `cat` in a PTY with echo off, so every byte the app writes comes back only
//!    after the process read and wrote it. That is the app's own read/write path
//!    (write → PTY → process → PTY → reader thread → channel) with nothing else
//!    in the way.
//! 2. The same thing through the real attach path: a scratch tmux server
//!    (`-L lmux-bench-<pid>`, never the app's own socket) with one window running
//!    `cat`, attached with [`lmux_app::pty::attach_argv`].
//!
//! Both wait for the pane to be in raw mode before measuring — until `stty raw`
//! has run, the line discipline echoes the byte itself and the measurement would
//! time the kernel, not the path.
//!
//! Run with `cargo test -p lmux-app --test localpty_bench -- --nocapture` to see
//! the numbers.

use std::time::{Duration, Instant};

use lmux_app::pty::{attach_argv, kill_scratch_server, tmux_bin, LocalPty};

/// Kills the scratch server however the bench ends.
struct Scratch<'a>(&'a str, &'a str);

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        kill_scratch_server(self.0, self.1);
    }
}

const ITERATIONS: usize = 200;
/// Printed by the pane command once the line discipline stops echoing.
const READY: &[u8] = b"READY";
const PANE_CMD: &str = "stty raw; printf READY; exec cat";

/// One byte out, the same byte back, timed.
fn measure(pty: &mut LocalPty, iterations: usize, label: &str) -> Vec<Duration> {
    // Warm up: the first bytes pay for the process starting and the first repaint.
    for _ in 0..20 {
        round_trip(pty, b'~');
    }
    let mut samples = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let byte = b'a' + (i % 26) as u8;
        samples.push(round_trip(pty, byte));
    }
    samples.sort();
    let at = |p: f64| samples[((samples.len() - 1) as f64 * p) as usize];
    println!(
        "{label}: n={} p50={:?} p90={:?} max={:?}",
        samples.len(),
        at(0.50),
        at(0.90),
        *samples.last().unwrap(),
    );
    samples
}

fn round_trip(pty: &mut LocalPty, byte: u8) -> Duration {
    // Anything still queued belongs to an earlier byte; the next chunk after the
    // write is this one's echo.
    while pty.wait_chunk(Duration::from_millis(0)).is_some() {}
    let start = Instant::now();
    pty.write(&[byte]).expect("write to pty");
    let deadline = start + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "no echo for {:?}", byte as char);
        match pty.wait_chunk(left) {
            Some(chunk) if chunk.contains(&byte) => return start.elapsed(),
            Some(_) => continue,
            None => panic!("no echo for {:?}", byte as char),
        }
    }
}

#[test]
fn keystroke_to_echo_direct() {
    let mut pty = match LocalPty::spawn_argv(
        "/bin/sh",
        &["-c".to_owned(), PANE_CMD.to_owned()],
        80,
        24,
        None,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping: could not open a PTY ({e})");
            return;
        }
    };
    assert!(wait_for_bytes(&mut pty, READY, Duration::from_secs(5)), "pane never went raw");
    let samples = measure(&mut pty, ITERATIONS, "pty + cat (no tmux)");
    let p50 = samples[samples.len() / 2];
    assert!(
        p50 < Duration::from_millis(1),
        "p50 {p50:?} is above the 1 ms target"
    );
}

#[test]
fn keystroke_to_echo_through_tmux_attach() {
    let socket = format!("lmux-bench-{}", std::process::id());
    let tmux = tmux_bin();
    if !tmux_available(&tmux) {
        eprintln!("skipping: tmux is not available");
        return;
    }
    let _ = run(&tmux, &["-L", &socket, "kill-server"]);
    if let Err(e) = run(
        &tmux,
        &["-L", &socket, "new-session", "-d", "-s", "lmux", "-x", "80", "-y", "24", "sh", "-c", PANE_CMD],
    ) {
        eprintln!("skipping: could not start a scratch tmux server ({e})");
        return;
    }
    let _scratch = Scratch(&tmux, &socket);
    let pane = match run(&tmux, &["-L", &socket, "display-message", "-p", "-t", "lmux", "#{pane_id}"])
    {
        Ok(out) => out.trim().to_owned(),
        Err(e) => {
            eprintln!("skipping: no pane id ({e})");
            return;
        }
    };
    assert!(pane.starts_with('%'), "unexpected pane id {pane:?}");
    // The attach argv is the one the app builds for a local pane.
    assert!(attach_argv(&socket, "surface:1", &pane).contains(&pane.to_owned()));

    let mut pty = match LocalPty::spawn_on(&socket, "surface:1", &pane, 80, 24, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping: could not attach ({e})");
            return;
        }
    };
    // The attach has taken: the view session exists and is the client's session.
    assert!(wait_for_session(&tmux, &socket, "view-surface-1", Duration::from_secs(5)),
        "the attach never appeared");
    // The pane is in raw mode: from here the echo is the process's, not the kernel's.
    assert!(wait_for_pane_text(&tmux, &socket, &pane, "READY", Duration::from_secs(5)),
        "the pane never went raw");

    let samples = measure(&mut pty, ITERATIONS, "pty + tmux attach + cat");

    // The view session is one per surface, and it goes with its client.
    assert!(run(&tmux, &["-L", &socket, "list-sessions", "-F", "#{session_name}"])
        .unwrap_or_default()
        .lines()
        .any(|s| s == "view-surface-1"));
    drop(pty);
    std::thread::sleep(Duration::from_millis(300));
    let sessions = run(&tmux, &["-L", &socket, "list-sessions", "-F", "#{session_name}"]).unwrap_or_default();
    assert!(
        !sessions.lines().any(|s| s == "view-surface-1"),
        "destroy-unattached should have taken the view session with the client: {sessions:?}"
    );

    let p50 = samples[samples.len() / 2];
    println!("(informational) tmux attach p50 = {p50:?}");
}

/// Wait until `needle` shows up in the PTY's output.
fn wait_for_bytes(pty: &mut LocalPty, needle: &[u8], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Some(chunk) = pty.wait_chunk(Duration::from_millis(50)) {
            seen.extend_from_slice(&chunk);
            if seen.windows(needle.len()).any(|w| w == needle) {
                return true;
            }
        }
    }
    false
}

fn wait_for_session(tmux: &str, socket: &str, name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let sessions = run(tmux, &["-L", socket, "list-sessions", "-F", "#{session_name}"]).unwrap_or_default();
        if sessions.lines().any(|s| s == name) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Ask tmux for the pane's screen, which is the only reliable way to see that the
/// pane's own command has run: tmux re-renders what we wrote, so the bytes on the
/// wire are not the pane's text verbatim.
fn wait_for_pane_text(tmux: &str, socket: &str, pane: &str, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let out = run(tmux, &["-L", socket, "capture-pane", "-p", "-t", pane]).unwrap_or_default();
        if out.contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn tmux_available(tmux: &str) -> bool {
    std::process::Command::new(tmux)
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run(tmux: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(tmux)
        .args(args)
        .output()
        .map_err(|e| format!("{tmux}: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
