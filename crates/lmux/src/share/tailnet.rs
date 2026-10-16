//! This machine's tailnet address, so a share is also reachable over
//! Tailscale at an address that survives restarts (a Quick Tunnel's does
//! not). Phones on the tailnet use it first and the tunnel as the fallback.
//!
//! Plain HTTP: WireGuard already encrypts the hop, and self-hosted control
//! servers (Headscale) issue no HTTPS certificates for MagicDNS names.

use std::net::Ipv4Addr;
use std::time::Duration;

use serde_json::Value;

/// The port the share listens on at the tailnet address. Fixed, so the
/// address a phone saved still works after the share restarts; any free port
/// when it is taken.
pub const PORT: u16 = 7871;

#[derive(Debug, Clone, PartialEq)]
pub struct Tailnet {
    pub ip: Ipv4Addr,
    /// The MagicDNS name, without the trailing dot.
    pub dns: Option<String>,
}

impl Tailnet {
    /// The share's tailnet URLs on `port`, the name first.
    pub fn urls(&self, port: u16) -> Vec<String> {
        let mut urls: Vec<String> = self.dns.iter().map(|d| format!("http://{d}:{port}")).collect();
        urls.push(format!("http://{}:{port}", self.ip));
        urls
    }
}

/// Tailscale's CLI, wherever this platform keeps it.
fn binaries() -> Vec<String> {
    let mut b = vec!["tailscale".to_string()];
    if cfg!(target_os = "macos") {
        b.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
    }
    b.extend(["/usr/bin/tailscale".into(), "/usr/local/bin/tailscale".into(), "/opt/homebrew/bin/tailscale".into()]);
    b
}

/// This machine on the tailnet, when Tailscale is up. `LMUX_SHARE_TAILNET=0`
/// turns it off.
pub async fn detect() -> Option<Tailnet> {
    if std::env::var("LMUX_SHARE_TAILNET").is_ok_and(|v| v == "0") {
        return None;
    }
    for bin in binaries() {
        let run = tokio::process::Command::new(&bin)
            .args(["status", "--json", "--self=true", "--peers=false"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(5), run).await else { continue };
        if !out.status.success() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { continue };
        return parse_status(&v);
    }
    None
}

/// `tailscale status --json`, read: only a running node with an IPv4
/// tailnet address counts.
pub fn parse_status(v: &Value) -> Option<Tailnet> {
    if v["BackendState"].as_str() != Some("Running") {
        return None;
    }
    let me = &v["Self"];
    let ip = me["TailscaleIPs"]
        .as_array()?
        .iter()
        .filter_map(|ip| ip.as_str()?.parse::<Ipv4Addr>().ok())
        .find(in_tailnet_range)?;
    let dns = me["DNSName"].as_str().map(|d| d.trim_end_matches('.').to_string()).filter(|d| !d.is_empty());
    Some(Tailnet { ip, dns })
}

/// 100.64.0.0/10, the CGNAT range Tailscale hands out.
pub fn in_tailnet_range(ip: &Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_running_node_and_its_magicdns_name() {
        let v = json!({"BackendState": "Running", "Self": {
            "DNSName": "lint-hqc-03.hs.lint.internal.", "TailscaleIPs": ["fd7a:115c:a1e0::2", "100.64.0.2"]}});
        let t = parse_status(&v).unwrap();
        assert_eq!(t, Tailnet { ip: "100.64.0.2".parse().unwrap(), dns: Some("lint-hqc-03.hs.lint.internal".into()) });
        assert_eq!(t.urls(7871), ["http://lint-hqc-03.hs.lint.internal:7871", "http://100.64.0.2:7871"]);
        assert!(parse_status(&json!({"BackendState": "Stopped", "Self": v["Self"]})).is_none());
        assert!(parse_status(&json!({"BackendState": "Running", "Self": {"TailscaleIPs": ["192.168.1.4"]}})).is_none());
        assert!(in_tailnet_range(&"100.127.255.1".parse().unwrap()));
        assert!(!in_tailnet_range(&"100.128.0.1".parse().unwrap()));
    }
}
