//! Who may use a share: one-time codes, and sessions whose credentials roll
//! over while a phone is connected, so a share does not end out from under a
//! phone that keeps using it.
//!
//! A pairing code turns into a *lineage*: the phone's standing with this
//! share. A lineage holds one or more credentials (bearer tokens, stored here
//! only as SHA-256 hashes). While a client that asked for rollover is
//! connected, a credential older than `ROTATE_AFTER` gets a successor, sent
//! over the live socket. The old credential keeps working until the client
//! acknowledges the new one (by using it), plus `LINGER` for requests in
//! flight, or for `GRACE` when the client never does. A phone that went
//! offline in the middle therefore comes back with either credential.
//!
//! Revoking a lineage ends it at once: every credential goes, and sockets
//! that check it on their next tick close.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How long an unused pairing code works.
pub const CODE_TTL: u64 = 300;
/// A credential nobody used for this long lapses. A connected socket counts as
/// use, so this is how long a phone may stay away.
pub const SESSION_IDLE: u64 = 7 * 24 * 3600;
/// The hard end of one credential. Rollover replaces credentials long before.
pub const CRED_MAX: u64 = 30 * 24 * 3600;
/// A connected client gets a successor once its credential is this old.
pub const ROTATE_AFTER: u64 = 24 * 3600;
/// How long a credential with an unacknowledged successor keeps working.
pub const GRACE: u64 = 7 * 24 * 3600;
/// How long a credential works after its successor was acknowledged.
pub const LINGER: u64 = 120;
/// A successor offer not acknowledged by then is offered again.
pub const RESEND_AFTER: u64 = 300;

/// `ROTATE_AFTER`, or `LMUX_SHARE_ROTATE_SECS` to watch rollover happen
/// (tests, and checking a client by hand).
fn rotate_after() -> u64 {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("LMUX_SHARE_ROTATE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(ROTATE_AFTER))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Cred {
    id: String,
    /// SHA-256 of the token, hex.
    hash: String,
    issued: u64,
    expires: u64,
    last_used: u64,
    /// Set when a successor exists: the end of this credential's overlap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retire_at: Option<u64>,
    /// The credential this one succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    predecessor: Option<String>,
    /// The client used this credential, so its predecessor can go.
    #[serde(default)]
    acked: bool,
}

impl Cred {
    fn valid(&self, now: u64) -> bool {
        self.expires > now && self.last_used + SESSION_IDLE > now && self.retire_at.is_none_or(|r| r > now)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Lineage {
    id: String,
    created: u64,
    creds: Vec<Cred>,
}

impl Lineage {
    fn alive(&self, now: u64) -> bool {
        self.creds.iter().any(|c| c.valid(now))
    }
}

#[derive(Debug, Clone, PartialEq)]
enum CodeKind {
    /// A QR code: starts a new lineage.
    Pair,
    /// Lets a page on another address of this share join the same lineage.
    Handoff(String),
    /// A web page's rollover: redeeming it makes the successor of `from`.
    Rollover { lineage: String, from: String },
}

#[derive(Debug, Clone)]
struct Code {
    expires: u64,
    kind: CodeKind,
}

/// A socket's or request's standing: its lineage and the credential it used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    pub lineage: String,
    pub cred: String,
}

/// A credential handed out: the token is shown once and never stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Granted {
    pub token: String,
    pub who: Who,
    /// Seconds until the credential's hard end, for the cookie's Max-Age.
    pub max_age: u64,
}

#[derive(Default)]
struct State {
    codes: HashMap<String, Code>,
    lineages: HashMap<String, Lineage>,
    /// When each credential was last offered a successor (not persisted).
    offered: HashMap<String, u64>,
}

pub struct Auth {
    state: Mutex<State>,
    /// Where lineages persist (the gateway service); `None` keeps them in
    /// memory, so stopping the share ends every session.
    store: Option<PathBuf>,
}

fn hash(token: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn short_id() -> String {
    super::server::random_token()[..12].to_string()
}

impl Auth {
    /// Sessions live as long as this process.
    pub fn in_memory() -> Self {
        Auth { state: Mutex::new(State::default()), store: None }
    }

    /// Sessions persist in `path` (mode 0600, hashes only), so a restarted
    /// gateway keeps its phones.
    pub fn persistent(path: PathBuf) -> Self {
        let lineages = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<Lineage>>(&t).ok())
            .unwrap_or_default();
        let now = now();
        let lineages = lineages.into_iter().filter(|l| l.alive(now)).map(|l| (l.id.clone(), l)).collect();
        Auth { state: Mutex::new(State { lineages, ..Default::default() }), store: Some(path) }
    }

    fn save(&self, state: &State) {
        let Some(path) = &self.store else { return };
        let mut list: Vec<&Lineage> = state.lineages.values().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        if let Ok(text) = serde_json::to_string(&list) {
            let _ = write_private(path, &text);
        }
    }

    fn new_cred(l: &mut Lineage, now: u64, predecessor: Option<String>) -> Granted {
        let token = super::server::random_token();
        let cred = Cred {
            id: short_id(),
            hash: hash(&token),
            issued: now,
            expires: now + CRED_MAX,
            last_used: now,
            retire_at: None,
            predecessor,
            acked: false,
        };
        let who = Who { lineage: l.id.clone(), cred: cred.id.clone() };
        l.creds.push(cred);
        Granted { token, who, max_age: CRED_MAX }
    }

    fn prune(state: &mut State, now: u64) {
        state.codes.retain(|_, c| c.expires > now);
        for l in state.lineages.values_mut() {
            l.creds.retain(|c| c.valid(now));
        }
        state.lineages.retain(|_, l| !l.creds.is_empty());
    }

    // --- codes ----------------------------------------------------------

    pub fn new_code(&self) -> String {
        self.new_code_at(now())
    }

    fn new_code_at(&self, now: u64) -> String {
        let code = super::server::random_token();
        self.state.lock().unwrap().codes.insert(code.clone(), Code { expires: now + CODE_TTL, kind: CodeKind::Pair });
        code
    }

    /// Whether a pairing code is still waiting to be scanned.
    pub fn code_live(&self, code: &str) -> bool {
        let now = now();
        let mut s = self.state.lock().unwrap();
        s.codes.retain(|_, c| c.expires > now);
        s.codes.keys().any(|k| ct_eq(k.as_bytes(), code.as_bytes()))
    }

    /// Spend a code: a pairing code starts a lineage, a handoff or rollover
    /// code adds a credential to an existing one.
    pub fn redeem(&self, code: &str) -> Option<Granted> {
        self.redeem_at(code, now())
    }

    fn redeem_at(&self, code: &str, now: u64) -> Option<Granted> {
        let mut s = self.state.lock().unwrap();
        Self::prune(&mut s, now);
        let hit = s.codes.keys().find(|k| ct_eq(k.as_bytes(), code.as_bytes())).cloned()?;
        let kind = s.codes.remove(&hit)?.kind;
        let granted = match kind {
            CodeKind::Pair => {
                let mut l = Lineage { id: short_id(), created: now, creds: Vec::new() };
                let g = Self::new_cred(&mut l, now, None);
                s.lineages.insert(l.id.clone(), l);
                g
            }
            CodeKind::Handoff(lineage) => {
                let l = s.lineages.get_mut(&lineage)?;
                Self::new_cred(l, now, None)
            }
            CodeKind::Rollover { lineage, from } => {
                let l = s.lineages.get_mut(&lineage)?;
                Self::succeed(l, &from, now)?
            }
        };
        self.save(&s);
        Some(granted)
    }

    /// Make the successor of credential `from`: any earlier successor that
    /// was never used goes (its client did not keep it), and `from` works for
    /// `GRACE` more unless the new one is used first.
    fn succeed(l: &mut Lineage, from: &str, now: u64) -> Option<Granted> {
        let old = l.creds.iter_mut().find(|c| c.id == from && c.valid(now))?;
        // Offered over a live socket: the old one is in use right now.
        old.last_used = now;
        old.retire_at = Some(old.retire_at.map_or(now + GRACE, |r| r.min(now + GRACE)));
        l.creds.retain(|c| c.predecessor.as_deref() != Some(from) || c.acked);
        Some(Self::new_cred(l, now, Some(from.to_string())))
    }

    // --- credentials ----------------------------------------------------

    /// The standing of `token`, keeping it alive. Using a successor is its
    /// acknowledgement: its predecessor only lingers briefly after.
    pub fn check(&self, token: &str) -> Option<Who> {
        self.check_at(token, now())
    }

    fn check_at(&self, token: &str, now: u64) -> Option<Who> {
        if token.is_empty() {
            return None;
        }
        let h = hash(token);
        let mut s = self.state.lock().unwrap();
        let mut found: Option<(String, usize)> = None;
        for l in s.lineages.values() {
            if let Some(i) = l.creds.iter().position(|c| ct_eq(c.hash.as_bytes(), h.as_bytes())) {
                found = Some((l.id.clone(), i));
            }
        }
        let (lid, i) = found?;
        let l = s.lineages.get_mut(&lid)?;
        if !l.creds[i].valid(now) {
            return None;
        }
        let quiet = now.saturating_sub(l.creds[i].last_used);
        l.creds[i].last_used = now;
        let mut changed = quiet >= 3600;
        if !l.creds[i].acked {
            l.creds[i].acked = true;
            changed = true;
            if let Some(p) = l.creds[i].predecessor.clone() {
                if let Some(old) = l.creds.iter_mut().find(|c| c.id == p) {
                    old.retire_at = Some(old.retire_at.map_or(now + LINGER, |r| r.min(now + LINGER)));
                }
            }
        }
        let who = Who { lineage: lid, cred: l.creds[i].id.clone() };
        if changed {
            self.save(&s);
        }
        Some(who)
    }

    /// For a connected socket: its credential, or the successor it rolled
    /// over to, kept alive. `None` when the lineage was revoked or lapsed.
    pub fn follow(&self, who: &Who) -> Option<Who> {
        self.follow_at(who, now())
    }

    fn follow_at(&self, who: &Who, now: u64) -> Option<Who> {
        let mut s = self.state.lock().unwrap();
        let l = s.lineages.get_mut(&who.lineage)?;
        let mut cur = who.cred.clone();
        // Bounded walk down the successor chain.
        for _ in 0..=l.creds.len() {
            if let Some(c) = l.creds.iter_mut().find(|c| c.id == cur && c.valid(now)) {
                // Saved now and then, so a restart does not forget a phone
                // that stayed connected the whole time.
                let quiet = now.saturating_sub(c.last_used);
                c.last_used = now;
                if quiet >= 3600 {
                    self.save(&s);
                }
                return Some(Who { lineage: who.lineage.clone(), cred: cur });
            }
            let next = l.creds.iter().filter(|c| c.predecessor.as_deref() == Some(cur.as_str()) && c.valid(now)).max_by_key(|c| c.issued)?;
            cur = next.id.clone();
        }
        None
    }

    /// Whether the socket's credential should be offered a successor now.
    pub fn rollover_due(&self, who: &Who) -> bool {
        self.rollover_due_at(who, now())
    }

    fn rollover_due_at(&self, who: &Who, now: u64) -> bool {
        let s = self.state.lock().unwrap();
        let Some(c) = s.lineages.get(&who.lineage).and_then(|l| l.creds.iter().find(|c| c.id == who.cred)) else {
            return false;
        };
        let recently = s.offered.get(&c.id).is_some_and(|t| now < t + RESEND_AFTER);
        c.valid(now) && now.saturating_sub(c.issued) >= rotate_after() && !recently
    }

    /// The app's rollover: a successor token, sent over the socket.
    pub fn offer_token(&self, who: &Who) -> Option<Granted> {
        self.offer_token_at(who, now())
    }

    fn offer_token_at(&self, who: &Who, now: u64) -> Option<Granted> {
        let mut s = self.state.lock().unwrap();
        s.offered.insert(who.cred.clone(), now);
        let l = s.lineages.get_mut(&who.lineage)?;
        let g = Self::succeed(l, &who.cred, now)?;
        self.save(&s);
        Some(g)
    }

    /// A web page's rollover: a one-time code it trades at `/auth` for the
    /// successor cookie, so the token never reaches page script.
    pub fn offer_code(&self, who: &Who) -> String {
        let now = now();
        let code = super::server::random_token();
        let mut s = self.state.lock().unwrap();
        s.offered.insert(who.cred.clone(), now);
        let kind = CodeKind::Rollover { lineage: who.lineage.clone(), from: who.cred.clone() };
        s.codes.insert(code.clone(), Code { expires: now + CODE_TTL, kind });
        code
    }

    /// A one-time code that signs a page in on another address of this share
    /// (the tailnet one, or a new tunnel address), into the same lineage. One
    /// per lineage: a new one replaces the last.
    pub fn handoff(&self, who: &Who) -> String {
        let now = now();
        let code = super::server::random_token();
        let mut s = self.state.lock().unwrap();
        s.codes.retain(|_, c| c.kind != CodeKind::Handoff(who.lineage.clone()));
        s.codes.insert(code.clone(), Code { expires: now + GRACE, kind: CodeKind::Handoff(who.lineage.clone()) });
        code
    }

    // --- management -----------------------------------------------------

    /// End a lineage (or every one) now. Returns how many ended.
    pub fn revoke(&self, lineage: Option<&str>) -> usize {
        let mut s = self.state.lock().unwrap();
        let before = s.lineages.len();
        match lineage {
            Some(id) => {
                s.lineages.remove(id);
                s.codes.retain(|_, c| !matches!(&c.kind, CodeKind::Handoff(l) | CodeKind::Rollover { lineage: l, .. } if l == id));
            }
            None => {
                s.lineages.clear();
                s.codes.retain(|_, c| c.kind == CodeKind::Pair);
            }
        }
        let n = before - s.lineages.len();
        self.save(&s);
        n
    }

    /// Every live lineage, for `lmux share-sessions`. No tokens or hashes.
    pub fn list(&self) -> Vec<Value> {
        let now = now();
        let mut s = self.state.lock().unwrap();
        Self::prune(&mut s, now);
        let mut out: Vec<Value> = s
            .lineages
            .values()
            .map(|l| {
                let last = l.creds.iter().map(|c| c.last_used).max().unwrap_or(l.created);
                let ends = l.creds.iter().filter(|c| c.valid(now)).map(|c| c.retire_at.unwrap_or(c.expires).min(c.last_used + SESSION_IDLE)).max();
                json!({"id": l.id, "created": l.created, "last_used": last, "credentials": l.creds.len(), "ends_if_unused": ends})
            })
            .collect();
        out.sort_by_key(|v| v["created"].as_u64());
        out
    }
}

fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    std::io::Write::write_all(&mut f, text.as_bytes())?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000;

    fn paired(a: &Auth) -> Granted {
        let code = a.new_code_at(T0);
        a.redeem_at(&code, T0).expect("pairs")
    }

    #[test]
    fn a_code_works_once_and_expires() {
        let a = Auth::in_memory();
        let c = a.new_code_at(T0);
        assert!(a.redeem_at("wrong", T0).is_none());
        let g = a.redeem_at(&c, T0 + 10).expect("first use");
        assert_eq!(a.check_at(&g.token, T0 + 11).unwrap(), g.who);
        assert!(a.redeem_at(&c, T0 + 12).is_none(), "second use must fail");
        let late = a.new_code_at(T0);
        assert!(a.redeem_at(&late, T0 + CODE_TTL + 1).is_none());
    }

    #[test]
    fn an_unused_session_lapses_and_a_used_one_does_not() {
        let a = Auth::in_memory();
        let g = paired(&a);
        let mut t = T0;
        for _ in 0..3 {
            t += SESSION_IDLE - 60;
            assert!(a.check_at(&g.token, t).is_some(), "use keeps it alive");
        }
        assert!(a.check_at(&g.token, t + SESSION_IDLE + 1).is_none(), "idle too long");
        // Without rollover (an older client), one credential ends at CRED_MAX.
        let old = paired(&a);
        let mut t = T0;
        while t + SESSION_IDLE < T0 + CRED_MAX {
            t += SESSION_IDLE - 60;
            a.check_at(&old.token, t);
        }
        assert!(a.check_at(&old.token, T0 + CRED_MAX).is_none());
    }

    #[test]
    fn rollover_carries_a_used_session_past_the_old_hard_end() {
        let a = Auth::in_memory();
        let mut g = paired(&a);
        let mut t = T0;
        // Ninety days, connected a few minutes a day, rolling over as due.
        while t < T0 + 90 * 24 * 3600 {
            t += 24 * 3600;
            let who = a.check_at(&g.token, t).expect("still paired");
            if a.rollover_due_at(&who, t) {
                g = a.offer_token_at(&who, t).unwrap();
                assert!(a.check_at(&g.token, t + 1).is_some());
            }
        }
    }

    #[test]
    fn rollover_keeps_both_until_the_successor_is_used() {
        let a = Auth::in_memory();
        let g = paired(&a);
        assert!(!a.rollover_due_at(&g.who, T0 + 60), "too young");
        let t = T0 + ROTATE_AFTER;
        assert!(a.rollover_due_at(&g.who, t));
        let next = a.offer_token_at(&g.who, t).unwrap();
        assert!(!a.rollover_due_at(&g.who, t + 1), "just offered");
        // Both work in the overlap; the socket still follows the old one.
        assert!(a.check_at(&g.token, t + 10).is_some());
        assert_eq!(a.follow_at(&g.who, t + 10).unwrap(), g.who);
        // Using the successor acknowledges it: the old one lingers, then ends.
        assert_eq!(a.check_at(&next.token, t + 20).unwrap(), next.who);
        assert!(a.check_at(&g.token, t + 20 + LINGER - 1).is_some());
        assert!(a.check_at(&g.token, t + 20 + LINGER + 1).is_none());
        // A socket opened with the old one moves on to the successor.
        assert_eq!(a.follow_at(&g.who, t + 20 + LINGER + 1).unwrap(), next.who);
    }

    #[test]
    fn a_phone_offline_during_rollover_recovers_with_either_credential() {
        let a = Auth::in_memory();
        let g = paired(&a);
        let t = T0 + ROTATE_AFTER;
        let next = a.offer_token_at(&g.who, t).unwrap();
        // It never acknowledged: the old one works for the grace period.
        assert!(a.check_at(&g.token, t + GRACE - 60).is_some());
        // Back within the grace period with the successor it had saved.
        let b = Auth::in_memory();
        let g2 = paired(&b);
        let next2 = b.offer_token_at(&g2.who, t).unwrap();
        assert!(b.check_at(&next2.token, t + GRACE - 60).is_some());
        assert!(b.check_at(&g2.token, t + GRACE - 60 + LINGER + 1).is_none());
        // Past the grace period with only the old one: over. Away longer
        // than SESSION_IDLE, the unused successor has lapsed as well.
        assert!(a.check_at(&g.token, t + GRACE + 1).is_none());
        assert!(a.check_at(&next.token, t + SESSION_IDLE + 2).is_none());
    }

    #[test]
    fn a_reoffer_drops_the_successor_nobody_kept() {
        let a = Auth::in_memory();
        let g = paired(&a);
        let t = T0 + ROTATE_AFTER;
        let lost = a.offer_token_at(&g.who, t).unwrap();
        assert!(a.rollover_due_at(&g.who, t + RESEND_AFTER), "offered again once the resend window passes");
        let kept = a.offer_token_at(&g.who, t + RESEND_AFTER).unwrap();
        assert!(a.check_at(&lost.token, t + RESEND_AFTER + 1).is_none());
        assert!(a.check_at(&kept.token, t + RESEND_AFTER + 1).is_some());
        // The grace period does not restart with the second offer.
        assert!(a.check_at(&g.token, t + GRACE + 1).is_none());
    }

    #[test]
    fn a_web_rollover_code_makes_the_successor_when_redeemed() {
        let a = Auth::in_memory();
        let g = paired(&a);
        let code = a.offer_code(&g.who);
        let next = a.redeem(&code).expect("redeems");
        assert_eq!(next.who.lineage, g.who.lineage);
        assert!(a.redeem(&code).is_none(), "once");
        assert!(a.check(&next.token).is_some());
    }

    #[test]
    fn revoke_ends_a_lineage_at_once() {
        let a = Auth::in_memory();
        let g = paired(&a);
        let other = paired(&a);
        let hand = a.handoff(&g.who);
        assert_eq!(a.list().len(), 2);
        assert_eq!(a.revoke(Some(&g.who.lineage)), 1);
        assert!(a.check(&g.token).is_none());
        assert!(a.follow(&g.who).is_none(), "its sockets close");
        assert!(a.redeem(&hand).is_none(), "its handoff code is gone too");
        assert!(a.check(&other.token).is_some());
        assert_eq!(a.revoke(None), 1);
        assert!(a.check(&other.token).is_none());
    }

    #[test]
    fn a_handoff_code_joins_the_same_lineage_once() {
        let a = Auth::in_memory();
        let g = paired(&a);
        let first = a.handoff(&g.who);
        let second = a.handoff(&g.who);
        assert!(a.redeem(&first).is_none(), "replaced by the newer one");
        let h = a.redeem(&second).unwrap();
        assert_eq!(h.who.lineage, g.who.lineage);
        assert!(a.check(&g.token).is_some(), "the original address keeps working");
    }

    #[test]
    fn persistent_sessions_survive_a_restart_and_keep_no_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("share-sessions.json");
        let a = Auth::persistent(path.clone());
        let code = a.new_code();
        let g = a.redeem(&code).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&g.token), "only hashes on disk");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let b = Auth::persistent(path.clone());
        assert_eq!(b.check(&g.token).unwrap(), g.who);
        b.revoke(Some(&g.who.lineage));
        assert!(Auth::persistent(path).check(&g.token).is_none(), "a revoke persists too");
    }
}
