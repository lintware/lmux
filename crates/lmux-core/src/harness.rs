//! Content-free, bounded delivery observations. A terminal write acknowledges
//! transport only; every stronger claim needs independent harness evidence.

use std::collections::hash_map::RandomState;
use std::collections::VecDeque;
use std::hash::{BuildHasher, Hash, Hasher};

use serde::Serialize;
use serde_json::{json, Value};

pub const STALE_AFTER_MS: u64 = 15_000;
const MAX_MESSAGES: usize = 128;
const MAX_PREFIX: usize = 256;
static NEXT_HARNESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub source: &'static str,
    pub event: String,
    pub confidence: &'static str,
    pub observed_at_ms: u64,
}

impl Evidence {
    fn new(source: &'static str, event: &str, confidence: &'static str, now: u64) -> Self {
        Self {
            source,
            event: event.into(),
            confidence,
            observed_at_ms: now,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackedMessage {
    pub id: String,
    pub state: &'static str,
    pub terminal_accepted: bool,
    pub accepted_at_ms: Option<u64>,
    pub submit_requested_at_ms: Option<u64>,
    pub submitted_at_ms: Option<u64>,
    pub consumed_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub harness_prompt_id: Option<String>,
    pub harness_session_id: Option<String>,
    pub updated_at_ms: u64,
    pub evidence: Evidence,
    // Keyed, process-local fingerprints never leave memory or the daemon.
    #[serde(skip)]
    fingerprint: u64,
    #[serde(skip)]
    screen_fingerprint: u64,
    #[serde(skip)]
    prefixes: Vec<u64>,
    #[serde(skip)]
    correlatable: bool,
}

#[derive(Debug, Clone)]
pub struct Harness {
    pub configuration: crate::harness_config::Configuration,
    salt: RandomState,
    epoch: String,
    sequence: u64,
    pub session_id: Option<String>,
    active_prompt: Option<String>,
    pub lifecycle: &'static str,
    lifecycle_evidence: Option<Evidence>,
    queue_count: Option<usize>,
    queue_evidence: Option<Evidence>,
    queue_lower_bound: bool,
    pub messages: VecDeque<TrackedMessage>,
    dropped_messages: u64,
    last_screen_error_at_ms: Option<u64>,
    foreground_grok: Option<bool>,
    progress_evidence: Option<Evidence>,
    progress_events: u64,
    prompt_order: VecDeque<String>,
    session_ended: bool,
}

impl Default for Harness {
    fn default() -> Self {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            configuration: Default::default(),
            salt: RandomState::new(),
            epoch: format!(
                "{}-{epoch:x}-{}",
                std::process::id(),
                NEXT_HARNESS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            sequence: 0,
            session_id: None,
            active_prompt: None,
            lifecycle: "unknown",
            lifecycle_evidence: None,
            queue_count: None,
            queue_evidence: None,
            queue_lower_bound: true,
            messages: VecDeque::new(),
            dropped_messages: 0,
            last_screen_error_at_ms: None,
            foreground_grok: None,
            progress_evidence: None,
            progress_events: 0,
            prompt_order: VecDeque::new(),
            session_ended: false,
        }
    }
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Harness {
    fn hash(&self, text: &str) -> u64 {
        let mut hash = self.salt.build_hasher();
        text.hash(&mut hash);
        hash.finish()
    }

    fn push(&mut self, text: &str, now: u64) -> String {
        self.sequence += 1;
        let id = format!("message:{}:{}", self.epoch, self.sequence);
        let fingerprint = self.hash(text.trim_end_matches(['\r', '\n']));
        let text = normalized(text);
        let prefixes = text
            .chars()
            .take(MAX_PREFIX)
            .scan(String::new(), |p, c| {
                p.push(c);
                Some(self.hash(p))
            })
            .collect();
        if self.messages.len() == MAX_MESSAGES {
            self.messages.pop_front();
            self.dropped_messages += 1;
        }
        self.messages.push_back(TrackedMessage {
            id: id.clone(),
            state: "unknown",
            terminal_accepted: false,
            accepted_at_ms: None,
            submit_requested_at_ms: None,
            submitted_at_ms: None,
            consumed_at_ms: None,
            completed_at_ms: None,
            harness_prompt_id: None,
            harness_session_id: None,
            updated_at_ms: now,
            evidence: Evidence::new("terminal", "write_pending", "authoritative", now),
            fingerprint,
            screen_fingerprint: self.hash(&text),
            prefixes,
            correlatable: true,
        });
        id
    }

    /// Register before the write so a fast hook cannot outrun the receipt.
    pub fn begin_send(&mut self, text: &str, now: u64) -> Option<String> {
        (!text.trim().is_empty()).then(|| self.push(text, now))
    }

    pub fn finish_send(&mut self, id: &str, accepted: bool, submit: bool, now: u64) {
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == id) {
            m.terminal_accepted = accepted;
            if !accepted {
                m.correlatable = false;
            }
            if accepted {
                m.accepted_at_ms = Some(now);
                m.submit_requested_at_ms = submit.then_some(now);
            }
            if m.evidence.source == "terminal" {
                m.evidence = Evidence::new(
                    "terminal",
                    if accepted {
                        "input_accepted"
                    } else {
                        "write_failed"
                    },
                    "authoritative",
                    now,
                );
                m.updated_at_ms = now;
            }
        }
    }

    pub fn note_submit_key(&mut self, id: &str, now: u64) {
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == id) {
            m.submit_requested_at_ms = Some(now);
        }
    }

    pub fn screen_error(&mut self, now: u64) {
        self.last_screen_error_at_ms = Some(now);
    }

    pub fn observe_screen(&mut self, grok: bool, screen: &str, now: u64) {
        self.last_screen_error_at_ms = None;
        self.foreground_grok = Some(grok);
        if !grok {
            self.configuration.invalidate();
            self.queue_count = None;
            self.queue_evidence = None;
            self.lifecycle = "unknown";
            self.lifecycle_evidence = Some(Evidence::new(
                "process",
                "non_grok_foreground",
                "authoritative",
                now,
            ));
            for m in &mut self.messages {
                if m.state == "queued" {
                    m.state = "unknown";
                    m.updated_at_ms = now;
                    m.evidence =
                        Evidence::new("process", "non_grok_foreground", "authoritative", now);
                }
            }
            return;
        }
        let parsed = parse_screen(screen);
        if let Some(lifecycle) = parsed.lifecycle {
            let hook_fresh = self.lifecycle_evidence.as_ref().is_some_and(|e| {
                matches!(e.source, "grok_hook" | "grok_status_line")
                    && self.lifecycle != "unknown"
                    && now.saturating_sub(e.observed_at_ms) <= STALE_AFTER_MS
            });
            if !hook_fresh {
                self.lifecycle = lifecycle;
                self.lifecycle_evidence = Some(Evidence::new(
                    "screen",
                    "grok_status_marker",
                    "heuristic",
                    now,
                ));
            }
        }
        self.queue_count = parsed.count;
        self.queue_lower_bound = parsed.lower_bound;
        self.queue_evidence = Some(Evidence::new(
            "screen",
            if parsed.count.is_some() {
                "grok_queue_rows"
            } else {
                "queue_not_visible"
            },
            "heuristic",
            now,
        ));
        let matches: Vec<Option<String>> = parsed
            .rows
            .iter()
            .map(|row| {
                let truncated = row.ends_with('…') || row.ends_with("...");
                let prefix = normalized(row.trim_end_matches('…').trim_end_matches("..."));
                let len = prefix.chars().count();
                if !(8..=MAX_PREFIX).contains(&len) {
                    return None;
                }
                let hash = self.hash(&prefix);
                let candidates: Vec<_> = self
                    .messages
                    .iter()
                    .filter(|m| {
                        m.correlatable
                            && m.harness_prompt_id.is_none()
                            && if truncated {
                                m.prefixes.get(len - 1) == Some(&hash)
                            } else {
                                m.screen_fingerprint == hash
                            }
                    })
                    .collect();
                (candidates.len() == 1).then(|| candidates[0].id.clone())
            })
            .collect();
        for m in &mut self.messages {
            if matches.iter().flatten().any(|id| id == &m.id) {
                m.state = "queued";
                m.updated_at_ms = now;
                m.evidence = Evidence::new("screen", "unique_queue_prefix_match", "heuristic", now);
            } else if m.state == "queued" {
                // A row can disappear through deletion, clipping, editing or
                // submission. Only independent evidence can identify receipt.
                m.state = "unknown";
                m.updated_at_ms = now;
                m.evidence = Evidence::new(
                    "screen",
                    "queue_row_no_longer_identifiable",
                    "heuristic",
                    now,
                );
            }
        }
    }

    /// Accept only the sanitized, supported Grok hook/status-line envelope.
    pub fn observe_grok(&mut self, event: &Value, now: u64) -> Result<(), &'static str> {
        let name = event["event"].as_str().ok_or("event required")?;
        if !EVENTS.contains(&name) && name != "status_line" && name != "config_options" {
            return Err("unsupported Grok event");
        }
        let session = event["session_id"]
            .as_str()
            .filter(|s| valid_id(s))
            .ok_or("session_id required (max 256 bytes)")?;
        for key in [
            "prompt_id",
            "prompt",
            "trigger",
            "notification_type",
            "model_id",
            "effort",
        ] {
            if event
                .get(key)
                .is_some_and(|v| !v.is_null() && !v.is_string())
            {
                return Err("optional event fields must be strings or null");
            }
        }
        if event["prompt_id"].as_str().is_some_and(|id| !valid_id(id)) {
            return Err("invalid prompt_id");
        }
        if event
            .get("subagent")
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err("subagent must be a boolean");
        }
        let options = if name == "config_options" {
            let mut options = if self.session_id.as_deref() == Some(session) {
                self.configuration.clone()
            } else {
                Default::default()
            };
            options.observe_options(event, now)?;
            Some(options)
        } else {
            None
        };
        let prompt = event["prompt_id"].as_str();
        if event["subagent"].as_bool() == Some(true) {
            return Ok(());
        }
        if self.session_ended
            && self.session_id.as_deref() == Some(session)
            && name != "session_start"
        {
            return Ok(());
        }
        if self.session_id.as_deref() != Some(session) {
            if name != "session_start" && self.session_id.is_some() {
                return Err("session mismatch; await SessionStart");
            }
            let previous_session = self.session_id.is_some();
            self.session_id = Some(session.into());
            self.configuration = Default::default();
            self.progress_evidence = None;
            self.progress_events = 0;
            self.prompt_order.clear();
            self.active_prompt = None;
            self.queue_count = None;
            self.queue_evidence = None;
            for m in &mut self.messages {
                if previous_session {
                    m.correlatable = false;
                }
                m.state = "unknown";
                m.evidence = Evidence::new("grok_hook", "session_changed", "authoritative", now);
                m.updated_at_ms = now;
            }
        }
        if let Some(options) = options {
            self.configuration = options;
            if self.foreground_grok == Some(false) {
                self.configuration.invalidate();
            }
            return Ok(());
        }
        let source = if name == "status_line" {
            "grok_status_line"
        } else {
            "grok_hook"
        };
        let evidence = Evidence::new(source, name, "authoritative", now);
        if name == "user_prompt_submit" {
            if let Some(prompt) = prompt {
                if self.prompt_order.iter().any(|p| p == prompt)
                    && self.prompt_order.back().map(String::as_str) != Some(prompt)
                {
                    return Ok(());
                }
                self.remember_prompt(prompt);
                let existing = self.messages.iter().position(|m| {
                    m.harness_session_id.as_deref() == Some(session)
                        && m.harness_prompt_id.as_deref() == Some(prompt)
                });
                let hash = event["prompt"]
                    .as_str()
                    .map(|s| self.hash(s.trim_end_matches(['\r', '\n'])));
                let candidates: Vec<_> = self
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| {
                        m.correlatable
                            && m.harness_prompt_id.is_none()
                            && hash == Some(m.fingerprint)
                    })
                    .map(|(i, _)| i)
                    .collect();
                let index = existing.or_else(|| (candidates.len() == 1).then(|| candidates[0]));
                if let Some(index) = index {
                    let m = &mut self.messages[index];
                    if m.submitted_at_ms.is_none() {
                        m.submitted_at_ms = Some(now);
                    }
                    m.harness_prompt_id = Some(prompt.into());
                    m.harness_session_id = Some(session.into());
                    if m.consumed_at_ms.is_none() {
                        m.state = "submitted";
                    }
                    m.updated_at_ms = now;
                    m.evidence = evidence.clone();
                    m.evidence.confidence = "heuristic_content_correlation";
                }
                self.active_prompt = Some(prompt.into());
            }
            // This is a decision gate: another hook may still reject it.
            self.lifecycle = "unknown";
        } else if name == "status_line" {
            // Timer refreshes repeat the previous payload; they are no heartbeat.
            if event["trigger"] == "refresh_interval" {
                return Ok(());
            }
            if prompt.is_some_and(|prompt| {
                self.prompt_order.iter().any(|p| p == prompt)
                    && self.prompt_order.back().map(String::as_str) != Some(prompt)
            }) {
                return Ok(());
            }
            self.configuration.observe_status_line(event, now);
            if self.foreground_grok == Some(false) {
                self.configuration.invalidate();
            }
            if matches!(self.lifecycle, "compacting" | "waiting")
                && self.lifecycle_evidence.as_ref().is_some_and(|e| {
                    e.source == "grok_hook"
                        && matches!(e.event.as_str(), "pre_compact" | "notification")
                        && now.saturating_sub(e.observed_at_ms) <= STALE_AFTER_MS
                })
            {
                return Ok(());
            }
            if let Some(prompt) = prompt {
                self.remember_prompt(prompt);
            }
            if prompt.is_some() {
                self.progress_events += 1;
                self.progress_evidence = Some(evidence.clone());
            }
            self.active_prompt = prompt.map(String::from);
            self.lifecycle = if prompt.is_some() { "running" } else { "idle" };
            if let Some(m) = self.messages.iter_mut().find(|m| {
                m.harness_session_id.as_deref() == Some(session)
                    && m.harness_prompt_id.as_deref() == prompt
                    && prompt.is_some()
            }) {
                m.state = "in_progress";
                m.consumed_at_ms.get_or_insert(now);
                m.updated_at_ms = now;
                m.evidence = evidence.clone();
                m.evidence.confidence = "heuristic_content_correlation";
            }
        } else if name == "pre_compact" {
            self.lifecycle = "compacting";
        } else if name == "post_compact" {
            self.lifecycle = "unknown";
        } else if name == "session_start" || name == "session_end" {
            if name == "session_end" {
                self.configuration.invalidate();
                self.session_ended = true;
            } else {
                self.session_ended = false;
                self.foreground_grok = None;
            }
            self.lifecycle = "unknown";
            self.active_prompt = None;
        } else if name == "notification" {
            match event["notification_type"].as_str() {
                Some("permission_prompt") => self.lifecycle = "waiting",
                Some("idle_prompt") => {
                    self.lifecycle = "idle";
                    self.active_prompt = None;
                }
                _ => return Ok(()),
            }
        } else if matches!(name, "stop" | "stop_failure" | "stop_cancelled") {
            // End reports can arrive after a newer prompt's start. Stop is a
            // gate and can be followed by continuation: completion stays unknown.
            if let Some(m) = self.messages.iter_mut().find(|m| {
                m.harness_session_id.as_deref() == Some(session)
                    && prompt.is_some()
                    && m.harness_prompt_id.as_deref() == prompt
            }) {
                m.state = "unknown";
                m.updated_at_ms = now;
                m.evidence = evidence.clone();
            }
            if prompt.is_some() && self.active_prompt.as_deref() != prompt {
                return Ok(());
            }
            self.lifecycle = if name == "stop" { "unknown" } else { "idle" };
        } else {
            // Tool hooks prove session activity, but carry no promptId.
            self.progress_events += 1;
            self.progress_evidence = Some(evidence.clone());
            self.lifecycle = "running";
        }
        self.lifecycle_evidence = Some(evidence);
        Ok(())
    }

    fn remember_prompt(&mut self, prompt: &str) {
        if !self.prompt_order.iter().any(|p| p == prompt) {
            if self.prompt_order.len() == MAX_MESSAGES {
                self.prompt_order.pop_front();
            }
            self.prompt_order.push_back(prompt.into());
        }
    }

    pub fn snapshot(&self, surface: &str, now: u64) -> Value {
        let age = |e: &Option<Evidence>| e.as_ref().map(|e| now.saturating_sub(e.observed_at_ms));
        let stale = |e: &Option<Evidence>| age(e).is_none_or(|age| age > STALE_AFTER_MS);
        let lifecycle_stale =
            stale(&self.lifecycle_evidence) || self.last_screen_error_at_ms.is_some();
        let queue_stale = self.session_ended
            || stale(&self.queue_evidence)
            || self.last_screen_error_at_ms.is_some();
        let messages: Vec<_> = self
            .messages
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap();
                let stale = self.session_ended
                    || now.saturating_sub(m.updated_at_ms) > STALE_AFTER_MS
                    || (m.evidence.source == "screen" && self.last_screen_error_at_ms.is_some());
                v["stale"] = json!(stale);
                v["last_observed_state"] = json!(m.state);
                if stale {
                    v["state"] = json!("unknown");
                }
                v["actual_steering"] = Value::Null;
                v["model_receipt"] = json!("unknown");
                v["consumption_evidence"] = json!(if m.consumed_at_ms.is_some() {
                    "active_prompt_observed"
                } else {
                    "unknown"
                });
                v
            })
            .collect();
        json!({
            "schema_version": 1,
            "surface": surface, "harness": if self.foreground_grok == Some(true) || (self.foreground_grok.is_none() && self.session_id.is_some()) { "grok" } else { "unknown" },
            "session_id": self.session_id, "queried_at_ms": now, "stale_after_ms": STALE_AFTER_MS,
            "lifecycle": {"state": if lifecycle_stale { "unknown" } else { self.lifecycle },
                "last_observed_state": self.lifecycle, "evidence": self.lifecycle_evidence,
                "age_ms": age(&self.lifecycle_evidence), "stale": lifecycle_stale},
            "queue": {"count": if queue_stale { None } else { self.queue_count },
                "last_observed_count": self.queue_count, "lower_bound": self.queue_lower_bound,
                "evidence": self.queue_evidence, "age_ms": age(&self.queue_evidence), "stale": queue_stale,
                "send_now_available": if !queue_stale && self.queue_count.is_some_and(|n| n > 0) { Some(true) } else { None },
                "enter_required": null, "send_now_key": "enter", "send_now_may_interrupt": true},
            "progress": {"observations": self.progress_events, "last_evidence": self.progress_evidence},
            "configuration": self.configuration.snapshot(now),
            "messages": messages, "dropped_messages": self.dropped_messages,
            "retention": "daemon_memory_last_128_per_surface", "last_screen_error_at_ms": self.last_screen_error_at_ms,
            "guidance": "Terminal acceptance confirms input transport only. Grok may hold follow-ups while compacting, running or waiting. Enter on an empty focused composer sends one queued row now and may interrupt the turn. Inspect Grok before sending a key; automatic/targeted queue flush is unsupported. Missing rows do not prove consumption. Hooks and optional status-line forwarding improve receipt evidence; actual steering and task completion remain unknown."
        })
    }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}

pub const EVENTS: &[&str] = &[
    "session_start",
    "session_end",
    "user_prompt_submit",
    "pre_compact",
    "post_compact",
    "pre_tool_use",
    "post_tool_use",
    "post_tool_use_failure",
    "stop",
    "stop_failure",
    "stop_cancelled",
    "notification",
];

/// Strip every free-text/tool/answer field except the transient prompt needed
/// for local fingerprint correlation. Nothing from this envelope is persisted.
pub fn grok_envelope(payload: &Value, status_line: bool) -> Value {
    let name = payload["hookEventName"].as_str().unwrap_or("");
    json!({
        "event": if status_line { "status_line" } else { name },
        "session_id": payload.get("sessionId").or_else(|| payload.get("session_id")),
        "prompt_id": payload.get("promptId").or_else(|| payload.get("prompt_id")),
        "prompt": if name == "user_prompt_submit" { payload.get("prompt") } else { None },
        "notification_type": payload.get("notificationType"),
        "subagent": payload.get("subagentType").is_some_and(|v| !v.is_null()),
        "trigger": payload.get("trigger"),
        "model_id": if status_line { payload["model"].get("id") } else { None },
        "effort": if status_line { payload["effort"].get("level") } else { None },
    })
}

#[derive(Default, Debug)]
struct Screen<'a> {
    lifecycle: Option<&'static str>,
    count: Option<usize>,
    lower_bound: bool,
    rows: Vec<&'a str>,
}

fn parse_screen(screen: &str) -> Screen<'_> {
    let mut out = Screen {
        lower_bound: true,
        ..Default::default()
    };
    // Only inspect the bottom of the visible screen, where Grok renders its
    // composer and status. Never search scrollback for lifecycle assertions.
    let lines: Vec<_> = screen
        .lines()
        .rev()
        .take(24)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let footer = lines.iter().rposition(|line| {
        let line = line.to_ascii_lowercase();
        line.contains("enter") && line.contains("send now")
    });
    if let Some(footer) = footer {
        let lower = lines[footer].to_ascii_lowercase();
        if let Some((before, _)) = lower.split_once(" queued") {
            let number: String = before
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if let Ok(count) = number.parse::<usize>() {
                out.count = Some(count);
                out.lower_bound = false;
            }
        }
        // Queue rows form a contiguous numbered block directly above the hint.
        // Stop at the composer boundary or a numbering discontinuity.
        let mut previous = None;
        for line in lines[..footer].iter().rev() {
            let line = line
                .trim()
                .trim_start_matches(['│', '┃', '>', '›', ' '])
                .trim();
            let Some((number, text)) = line.split_once(". ").or_else(|| line.split_once(") "))
            else {
                break;
            };
            let Ok(number) = number.parse::<usize>() else {
                break;
            };
            if number == 0 || number > 10000 || previous.is_some_and(|p| number + 1 != p) {
                break;
            }
            if out.count.is_some_and(|count| number > count) {
                break;
            }
            out.rows.push(text.trim().trim_end_matches('│').trim());
            previous = Some(number);
            if number == 1 {
                break;
            }
        }
        out.rows.reverse();
        if out.count.is_none() && !out.rows.is_empty() {
            out.count = Some(out.rows.len());
        }
    }
    for line in lines.iter().rev().take(8) {
        let lower = line.to_ascii_lowercase();
        if lower.contains("compacting") && (lower.contains('…') || lower.contains("...")) {
            out.lifecycle = Some("compacting");
            break;
        }
        if lower.contains("esc to interrupt")
            || lower.contains("ctrl+c to interrupt")
            || lower.contains("ctrl+c to stop")
        {
            out.lifecycle = Some("running");
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_acp_observation_cannot_inherit_a_previous_unbound_request() {
        let mut h = Harness::default();
        h.configuration.request(Some("old-model"), None, 1);
        h.observe_grok(&json!({"event":"config_options", "session_id":"new-session", "config_options":[
            {"id":"model", "type":"select", "currentValue":"new-model", "options":[{"value":"new-model"}]}
        ]}), 2).unwrap();
        let v = h.snapshot("surface:1", 3);
        assert!(v["configuration"]["requested"].is_null());
        assert_eq!(v["configuration"]["applied"]["model"], "new-model");
    }

    #[test]
    fn active_prompt_keeps_fresh_waiting_evidence_and_new_grok_replaces_shell_evidence() {
        let mut h = Harness::default();
        hook(
            &mut h,
            "user_prompt_submit",
            Some("p1"),
            Some("prompt text"),
            1,
        );
        h.observe_grok(&json!({"event":"notification","session_id":"session-a","notification_type":"permission_prompt"}), 2).unwrap();
        h.observe_grok(
            &json!({"event":"status_line","session_id":"session-a","prompt_id":"p1"}),
            3,
        )
        .unwrap();
        assert_eq!(h.lifecycle, "waiting");
        hook(&mut h, "post_tool_use", None, None, 4);
        assert_eq!(h.lifecycle, "running");
        let mut h = Harness::default();
        h.observe_screen(false, "ordinary shell", 1);
        h.observe_screen(true, "Compacting...", 2);
        assert_eq!(h.lifecycle, "compacting");
    }

    #[test]
    fn internal_whitespace_and_failed_writes_cannot_borrow_a_receipt() {
        let mut h = Harness::default();
        send(&mut h, "print('a  b')\n", 1);
        hook(
            &mut h,
            "user_prompt_submit",
            Some("p1"),
            Some("print('a b')"),
            2,
        );
        h.observe_grok(
            &json!({"event":"status_line","session_id":"session-a","prompt_id":"p1"}),
            3,
        )
        .unwrap();
        assert!(h.messages[0].consumed_at_ms.is_none());
        assert!(h.messages[0].harness_prompt_id.is_none());
        let id = h.begin_send("failed input", 4).unwrap();
        h.finish_send(&id, false, true, 5);
        hook(
            &mut h,
            "user_prompt_submit",
            Some("p2"),
            Some("failed input"),
            6,
        );
        assert!(h.messages[1].harness_prompt_id.is_none());
    }

    #[test]
    fn malformed_events_leave_all_observations_unchanged() {
        let mut h = Harness::default();
        let before = h.snapshot("surface:1", 1);
        assert!(h
            .observe_grok(
                &json!({"event":"config_options","session_id":"wrong","config_options":{}}),
                1
            )
            .is_err());
        assert_eq!(h.snapshot("surface:1", 1), before);
        h.observe_grok(
            &json!({"event":"status_line","session_id":"session-a","prompt_id":"p1"}),
            2,
        )
        .unwrap();
        let before = h.snapshot("surface:1", 3);
        assert!(h
            .observe_grok(
                &json!({"event":"status_line","session_id":"session-a","prompt_id":123}),
                3
            )
            .is_err());
        assert_eq!(h.snapshot("surface:1", 3), before);
    }

    #[test]
    fn delayed_status_line_cannot_reactivate_an_older_prompt() {
        let mut h = Harness::default();
        hook(
            &mut h,
            "user_prompt_submit",
            Some("old"),
            Some("old text"),
            1,
        );
        hook(
            &mut h,
            "user_prompt_submit",
            Some("new"),
            Some("new text"),
            2,
        );
        h.observe_grok(&json!({"event":"status_line","session_id":"session-a","prompt_id":"new","model_id":"new-model"}), 3).unwrap();
        h.observe_grok(&json!({"event":"status_line","session_id":"session-a","prompt_id":"old","model_id":"old-model"}), 4).unwrap();
        assert_eq!(h.active_prompt.as_deref(), Some("new"));
        assert_eq!(h.configuration.snapshot(5)["applied"]["model"], "new-model");
        hook(&mut h, "stop_cancelled", Some("new"), None, 6);
        assert_eq!(h.lifecycle, "idle");
    }

    #[test]
    fn old_screen_compaction_cannot_veto_a_new_status_observation() {
        let mut h = Harness::default();
        h.observe_screen(true, "Compacting...", 1);
        h.observe_grok(
            &json!({"event":"status_line","session_id":"session-a","prompt_id":"p1"}),
            STALE_AFTER_MS + 2,
        )
        .unwrap();
        assert_eq!(
            h.snapshot("surface:1", STALE_AFTER_MS + 3)["lifecycle"]["state"],
            "running"
        );
    }

    #[test]
    fn complete_rows_require_equality_and_transcript_rows_stay_outside_queue() {
        let mut h = Harness::default();
        send(&mut h, "deploy production tomorrow", 1);
        h.observe_screen(true, "1. deploy production\n1 queued, Enter to send now", 2);
        assert_eq!(h.messages[0].state, "unknown");
        h.observe_screen(
            true,
            "1. deploy production…\n1 queued, Enter to send now",
            3,
        );
        assert_eq!(h.messages[0].state, "queued");
        h.observe_screen(true, "1. deploy production tomorrow\nAssistant response end\n1. unrelated queue text\n1 queued, Enter to send now", 4);
        assert_eq!(h.messages[0].state, "unknown");
        assert_eq!(h.snapshot("surface:1", 5)["queue"]["count"], 1);
    }

    #[test]
    fn ended_or_non_grok_session_invalidates_configuration() {
        let mut h = Harness::default();
        let status = json!({"event":"status_line","session_id":"session-a","prompt_id":"p1","model_id":"model-a","effort":"high"});
        h.observe_grok(&status, 1).unwrap();
        hook(&mut h, "session_end", None, None, 2);
        h.observe_grok(&status, 3).unwrap();
        let config = h.configuration.snapshot(4);
        assert!(config["applied"]["model"].is_null());
        assert_eq!(config["applied"]["last_observed_model"], "model-a");
        let mut h = Harness::default();
        h.observe_grok(&status, 1).unwrap();
        h.observe_screen(false, "ordinary shell", 2);
        assert!(h.configuration.snapshot(3)["applied"]["model"].is_null());
    }

    #[test]
    fn runtime_records_and_fingerprints_are_excluded_from_persistent_state() {
        let mut state = crate::tree::SessionState::default();
        let h = state.harnesses.entry("surface:1".into()).or_default();
        send(h, "PRIVATE_BODY", 1);
        assert!(!serde_json::to_string(&state).unwrap().contains("harness"));
        state.prune_resume();
        assert!(state.harnesses.is_empty());
    }

    fn send(h: &mut Harness, text: &str, now: u64) -> String {
        let id = h.begin_send(text, now).unwrap();
        h.finish_send(&id, true, true, now);
        id
    }

    fn hook(h: &mut Harness, event: &str, prompt_id: Option<&str>, prompt: Option<&str>, now: u64) {
        h.observe_grok(&json!({"event": event, "session_id": "session-a", "prompt_id": prompt_id, "prompt": prompt}), now).unwrap();
    }

    #[test]
    fn compaction_and_three_messages_need_independent_delivery_evidence() {
        let mut h = Harness::default();
        hook(&mut h, "session_start", None, None, 1);
        hook(&mut h, "pre_compact", None, None, 2);
        let a = send(&mut h, "steer alpha\n", 3);
        let b = send(&mut h, "steer bravo\n", 4);
        let c = send(&mut h, "steer charlie\n", 5);
        let v = h.snapshot("surface:1", 6);
        assert_eq!(v["lifecycle"]["state"], "compacting");
        assert!(v["queue"]["count"].is_null());
        assert!(v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["state"] == "unknown" && m["consumed_at_ms"].is_null()));
        h.observe_screen(
            true,
            "1. steer alpha\n2. steer bravo\n3. steer charlie\n3 queued, Enter to send now\n",
            7,
        );
        let v = h.snapshot("surface:1", 8);
        assert_eq!(v["queue"]["count"], 3);
        assert_eq!(v["queue"]["evidence"]["source"], "screen");
        assert_eq!(v["messages"][0]["state"], "queued");
        hook(&mut h, "post_compact", None, None, 8);
        hook(
            &mut h,
            "user_prompt_submit",
            Some("p1"),
            Some("steer alpha"),
            9,
        );
        h.observe_grok(
            &json!({"event":"status_line", "session_id":"session-a", "prompt_id":"p1"}),
            10,
        )
        .unwrap();
        h.observe_screen(
            true,
            "1. steer bravo\n2. steer charlie\n2 queued, Enter to send now\n",
            11,
        );
        let v = h.snapshot("surface:1", 12);
        assert_eq!(v["queue"]["count"], 2);
        assert_eq!(v["messages"][0]["id"], a);
        assert_eq!(v["messages"][0]["state"], "in_progress");
        assert_eq!(v["messages"][0]["consumed_at_ms"], 10);
        assert_eq!(v["messages"][1]["id"], b);
        assert_eq!(v["messages"][1]["state"], "queued");
        assert_eq!(v["messages"][2]["id"], c);
        hook(
            &mut h,
            "user_prompt_submit",
            Some("p2"),
            Some("steer bravo"),
            13,
        );
        h.observe_grok(
            &json!({"event":"status_line", "session_id":"session-a", "prompt_id":"p2"}),
            14,
        )
        .unwrap();
        h.observe_screen(true, "1. steer charlie\n1 queued, Enter to send now\n", 15);
        let v = h.snapshot("surface:1", 16);
        assert_eq!(v["queue"]["count"], 1);
        assert_eq!(v["messages"][1]["consumed_at_ms"], 14);
        assert!(v["messages"][2]["consumed_at_ms"].is_null());
        assert!(v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["actual_steering"].is_null() && m["completed_at_ms"].is_null()));
    }

    #[test]
    fn row_disappearance_and_stop_gate_do_not_prove_consumption_or_completion() {
        let mut h = Harness::default();
        send(&mut h, "steer alpha", 1);
        h.observe_screen(true, "1. steer alpha\nEnter to send now", 2);
        h.observe_screen(true, "hidden queue", 3);
        assert_eq!(h.messages[0].state, "unknown");
        assert!(h.messages[0].consumed_at_ms.is_none());
        hook(
            &mut h,
            "user_prompt_submit",
            Some("p1"),
            Some("steer alpha"),
            4,
        );
        assert_eq!(h.messages[0].state, "submitted");
        assert!(h.messages[0].consumed_at_ms.is_none());
        hook(&mut h, "stop", Some("p1"), None, 5);
        assert_eq!(h.messages[0].state, "unknown");
        assert!(h.messages[0].completed_at_ms.is_none());
    }

    #[test]
    fn stale_and_failed_capture_remove_current_claims_but_keep_timestamps() {
        let mut h = Harness::default();
        send(&mut h, "steer alpha", 1);
        h.observe_screen(
            true,
            "1. steer alpha\n1 queued, Enter to send now\nCompacting...",
            2,
        );
        let v = h.snapshot("surface:1", STALE_AFTER_MS + 3);
        assert_eq!(v["lifecycle"]["state"], "unknown");
        assert_eq!(v["lifecycle"]["last_observed_state"], "compacting");
        assert!(v["queue"]["count"].is_null());
        assert_eq!(v["queue"]["last_observed_count"], 1);
        assert_eq!(v["messages"][0]["state"], "unknown");
        assert_eq!(v["messages"][0]["last_observed_state"], "queued");
        h.screen_error(3);
        let v = h.snapshot("surface:1", 4);
        assert!(v["queue"]["count"].is_null());
        assert_eq!(v["messages"][0]["stale"], true);
    }

    #[test]
    fn ordinary_shell_output_cannot_be_mistaken_for_grok() {
        let mut h = Harness::default();
        send(&mut h, "echo secrets", 1);
        h.observe_screen(
            false,
            "1. echo secrets\n1 queued, Enter to send now\nCompacting...",
            2,
        );
        let v = h.snapshot("surface:1", 3);
        assert_eq!(v["harness"], "unknown");
        assert_eq!(v["lifecycle"]["state"], "unknown");
        assert!(v["queue"]["count"].is_null());
        assert_eq!(v["messages"][0]["state"], "unknown");
    }

    #[test]
    fn duplicate_or_short_prefixes_do_not_guess_message_identity() {
        let mut h = Harness::default();
        send(&mut h, "same prompt body", 1);
        send(&mut h, "same prompt body", 2);
        send(&mut h, "yes", 3);
        h.observe_screen(
            true,
            "1. same prompt body\n2. yes\n2 queued, Enter to send now",
            4,
        );
        assert!(h.messages.iter().all(|m| m.state == "unknown"));
        hook(
            &mut h,
            "user_prompt_submit",
            Some("p1"),
            Some("same prompt body"),
            5,
        );
        assert!(h.messages.iter().all(|m| m.harness_prompt_id.is_none()));
    }

    #[test]
    fn old_turn_ends_and_child_hooks_do_not_settle_a_new_turn() {
        let mut h = Harness::default();
        hook(&mut h, "user_prompt_submit", Some("old"), Some("old"), 1);
        hook(&mut h, "user_prompt_submit", Some("new"), Some("new"), 2);
        h.observe_grok(
            &json!({"event":"status_line", "session_id":"session-a", "prompt_id":"new"}),
            3,
        )
        .unwrap();
        hook(&mut h, "stop_cancelled", Some("old"), None, 4);
        assert_eq!(h.lifecycle, "running");
        h.observe_grok(
            &json!({"event":"stop_cancelled", "session_id":"child", "subagent":true}),
            5,
        )
        .unwrap();
        assert_eq!(h.lifecycle, "running");
        assert_eq!(h.session_id.as_deref(), Some("session-a"));
    }

    #[test]
    fn status_line_timer_is_not_a_heartbeat_and_new_session_cannot_reuse_receipts() {
        let mut h = Harness::default();
        send(&mut h, "steer alpha", 1);
        hook(
            &mut h,
            "user_prompt_submit",
            Some("p1"),
            Some("steer alpha"),
            2,
        );
        h.observe_grok(
            &json!({"event":"status_line", "session_id":"session-a", "prompt_id":"p1"}),
            3,
        )
        .unwrap();
        h.observe_grok(&json!({"event":"status_line", "session_id":"session-a", "prompt_id":"p1", "trigger":"refresh_interval"}), STALE_AFTER_MS + 10).unwrap();
        assert_eq!(
            h.snapshot("surface:1", STALE_AFTER_MS + 11)["lifecycle"]["stale"],
            true
        );
        h.observe_grok(
            &json!({"event":"session_start", "session_id":"session-b"}),
            STALE_AFTER_MS + 12,
        )
        .unwrap();
        h.observe_grok(&json!({"event":"user_prompt_submit", "session_id":"session-b", "prompt_id":"p1", "prompt":"steer alpha"}), STALE_AFTER_MS + 13).unwrap();
        assert_eq!(
            h.messages[0].harness_session_id.as_deref(),
            Some("session-a")
        );
        assert_eq!(h.messages[0].state, "unknown");
    }

    #[test]
    fn retention_is_bounded_and_serialization_has_no_content_or_fingerprints() {
        let mut h = Harness::default();
        for _ in 0..140 {
            send(&mut h, "VERY_SECRET_PROMPT_BODY", 1);
        }
        let v = h.snapshot("surface:1", 2);
        assert_eq!(v["messages"].as_array().unwrap().len(), MAX_MESSAGES);
        assert_eq!(v["dropped_messages"], 12);
        let serialized = v.to_string();
        assert!(!serialized.contains("VERY_SECRET"));
        assert!(!serialized.contains("fingerprint"));
        assert!(!serialized.contains("prefixes"));
        let envelope = grok_envelope(
            &json!({"hookEventName":"post_tool_use", "sessionId":"s", "toolInput":{"secret":"secret"}, "lastAssistantMessage":"secret"}),
            false,
        );
        assert!(!envelope.to_string().contains("secret"));
    }
}
