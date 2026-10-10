//! Usage limits read from a running OpenUsage (https://github.com/robinebers/openusage).
//!
//! OpenUsage serves every provider's limits as JSON on a loopback port. A
//! background thread polls it, so lmux shows exactly what OpenUsage shows (and
//! picks up providers it adds later) without re-implementing any provider.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;

const ADDR: &str = "127.0.0.1:6736";
const POLL: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub kind: String,
    pub unit: String,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub utilization: Option<f64>,
    pub available: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub display_name: String,
    pub plan: Option<String>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub resources: BTreeMap<String, Resource>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Envelope {
    #[serde(default)]
    providers: BTreeMap<String, Provider>,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    /// True while OpenUsage answered the last poll.
    pub online: bool,
    pub providers: Vec<(String, Provider)>,
}

impl Usage {
    /// The most-used bounded limit across all providers, as 0..=1.
    pub fn peak(&self) -> Option<f64> {
        self.providers
            .iter()
            .flat_map(|(_, p)| p.resources.values())
            .filter_map(|r| r.utilization)
            .reduce(f64::max)
    }
}

fn shared() -> &'static Arc<Mutex<Usage>> {
    static S: OnceLock<Arc<Mutex<Usage>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// The latest reading; `online` is false until OpenUsage has answered.
pub fn current() -> Usage {
    shared().lock().map(|u| u.clone()).unwrap_or_default()
}

/// Start the poller once. Each reading wakes the UI.
pub fn start(ctx: &egui::Context) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let ctx = ctx.clone();
    std::thread::spawn(move || loop {
        let next = match fetch() {
            Some(env) => Usage {
                online: true,
                providers: env.providers.into_iter().filter(|(_, p)| !p.resources.is_empty()).collect(),
            },
            None => Usage::default(),
        };
        if let Ok(mut u) = shared().lock() {
            *u = next;
        }
        ctx.request_repaint();
        std::thread::sleep(POLL);
    });
}

fn fetch() -> Option<Envelope> {
    let addr: SocketAddr = ADDR.parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(3))).ok()?;
    s.write_all(b"GET /v1/limits HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Option<Envelope> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    if !raw.starts_with(b"HTTP/1.1 200") && !raw.starts_with(b"HTTP/1.0 200") {
        return None;
    }
    serde_json::from_slice(&raw[split + 4..]).ok()
}

/// Seconds from `now_unix` until an ISO-8601 UTC instant like
/// `2026-10-10T16:10:00.262Z`; `None` if it cannot be read.
pub fn secs_until(iso: &str, now_unix: i64) -> Option<i64> {
    let b = iso.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| iso.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days since 1970-01-01 (Howard Hinnant's civil-from-days, inverted).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss - now_unix)
}

/// "2h 14m", "3d 4h", "now".
pub fn countdown(secs: i64) -> String {
    if secs <= 0 {
        return "now".into();
    }
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openusage_envelope() {
        let body = r#"{"schema":"openusage.limits.v1","errors":[],"providers":{"claude":{"displayName":"Claude","plan":"Max 5x","stale":false,"resources":{"session":{"kind":"consumption","unit":"percent","used":60,"limit":100,"utilization":0.6,"resetsAt":"2026-10-10T16:10:00.262Z"},"rateLimitResets":{"kind":"balance","unit":"resets","available":1}}}}}"#;
        let raw = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let env = parse_response(raw.as_bytes()).unwrap();
        let c = &env.providers["claude"];
        assert_eq!(c.plan.as_deref(), Some("Max 5x"));
        assert_eq!(c.resources["session"].utilization, Some(0.6));
        assert_eq!(c.resources["rateLimitResets"].available, Some(1.0));
    }

    #[test]
    fn rejects_non_200() {
        assert!(parse_response(b"HTTP/1.1 503 Busy\r\n\r\n{}").is_none());
    }

    #[test]
    fn iso_countdown() {
        // 2026-10-10T16:10:00Z is 1_791_648_600.
        let at = secs_until("2026-10-10T16:10:00.262Z", 1_791_648_600 - 3 * 3600 - 60).unwrap();
        assert_eq!(at, 3 * 3600 + 60);
        assert_eq!(countdown(at), "3h 1m");
        assert_eq!(countdown(4 * 86400 + 5 * 3600), "4d 5h");
    }
}
