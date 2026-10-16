//! Stamps the build with its commit, so `lmux --version` and the daemon's
//! ping say which source a host runs. `LMUX_BUILD_ID` wins (a remote build
//! from `git archive` has no `.git`; deploy/install-remote.sh passes it).

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=LMUX_BUILD_ID");
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for f in ["HEAD", "index", "refs/heads"] {
            println!("cargo:rerun-if-changed={dir}/{f}");
        }
    }
    let id = std::env::var("LMUX_BUILD_ID").ok().filter(|s| !s.is_empty()).or_else(|| {
        let head = git(&["rev-parse", "--short=8", "HEAD"])?;
        let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        Some(if dirty { format!("{head}-dirty") } else { head })
    });
    println!("cargo:rustc-env=LMUX_BUILD_ID={}", id.unwrap_or_else(|| "unknown".into()));
}
