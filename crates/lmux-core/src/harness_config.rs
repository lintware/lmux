//! Session configuration observations and fail-closed external control.
//! Grok documents ACP setters on the owning connection. An arbitrary terminal
//! has no documented passive attachment route, so sending slash commands is
//! deliberately excluded from this interface.

use crate::harness::{Evidence, STALE_AFTER_MS};
use serde_json::{json, Value};

const UNSUPPORTED: &str = "Grok's running terminal session exposes no supported external control attachment. Use an existing owning ACP connection with session/set_config_option and verify returned configOptions.currentValue. lmux sent no terminal input, cancelled no turn and changed no defaults.";

#[derive(Debug, Clone, Default)]
pub struct Configuration {
    model: Option<String>,
    effort: Option<String>,
    evidence: Option<Evidence>,
    models: Option<Vec<String>>,
    efforts: Option<Vec<String>>,
    options_evidence: Option<Evidence>,
    requested: Option<Value>,
    invalidated: bool,
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/:".contains(&b))
}

fn ids(options: &Value, out: &mut Vec<String>) {
    for option in options.as_array().into_iter().flatten().take(4096) {
        if let Some(id) = option["value"].as_str().filter(|s| identifier(s)) {
            if !out.iter().any(|x| x == id) && out.len() < 4096 {
                out.push(id.into());
            }
        } else if option["options"].is_array() {
            // Standard ACP select options can be grouped one level deep.
            for value in option["options"]
                .as_array()
                .into_iter()
                .flatten()
                .take(4096)
            {
                if let Some(id) = value["value"].as_str().filter(|s| identifier(s)) {
                    if !out.iter().any(|x| x == id) && out.len() < 4096 {
                        out.push(id.into());
                    }
                }
            }
        }
    }
}

impl Configuration {
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    pub fn observe_status_line(&mut self, event: &Value, now: u64) {
        self.invalidated = false;
        let model = event["model_id"]
            .as_str()
            .filter(|s| identifier(s))
            .map(String::from);
        if self.model != model {
            self.models = None;
            self.efforts = None;
            self.options_evidence = None;
        }
        self.model = model;
        self.effort = event["effort"]
            .as_str()
            .filter(|s| identifier(s))
            .map(String::from);
        self.evidence = Some(Evidence {
            source: "grok_status_line",
            event: "session_configuration".into(),
            confidence: "authoritative",
            observed_at_ms: now,
        });
    }

    /// Feed only a configOptions response/notification from the already owning
    /// ACP client. This is observation, and never opens or loads a session.
    pub fn observe_options(&mut self, event: &Value, now: u64) -> Result<(), &'static str> {
        let options = event["config_options"]
            .as_array()
            .ok_or("config_options must be an ACP configOptions array")?;
        let mut model = None;
        let mut effort = None;
        let mut models = None;
        let mut efforts = None;
        for option in options.iter().take(128) {
            if !matches!(option["id"].as_str(), Some("model" | "reasoning_effort")) {
                continue;
            }
            if option["type"] != "select"
                || !option["options"].is_array()
                || !option["currentValue"].as_str().is_some_and(identifier)
            {
                return Err("model/effort config options must be typed ACP select options");
            }
            let mut values = Vec::new();
            ids(&option["options"], &mut values);
            let current = option["currentValue"]
                .as_str()
                .filter(|s| identifier(s))
                .map(String::from);
            match option["id"].as_str() {
                Some("model") => {
                    model = current;
                    models = Some(values);
                }
                Some("reasoning_effort") => {
                    effort = current;
                    efforts = Some(values);
                }
                _ => {}
            }
        }
        self.invalidated = false;
        self.model = model;
        self.effort = effort;
        self.models = models;
        self.efforts = efforts;
        let evidence = Evidence {
            source: "grok_acp",
            event: "config_option_update".into(),
            confidence: "authoritative",
            observed_at_ms: now,
        };
        self.evidence = Some(evidence.clone());
        self.options_evidence = Some(evidence);
        Ok(())
    }

    pub fn request(&mut self, model: Option<&str>, effort: Option<&str>, now: u64) {
        let fresh_options = !self.invalidated
            && self
                .options_evidence
                .as_ref()
                .is_some_and(|e| now.saturating_sub(e.observed_at_ms) <= STALE_AFTER_MS);
        let invalid_syntax = model.into_iter().chain(effort).any(|s| !identifier(s));
        // The current model's effort menu cannot validate another model's effort.
        let effort_choices = if model.is_none() || model == self.model.as_deref() {
            &self.efforts
        } else {
            &None
        };
        let invalid_choice = fresh_options
            && [(model, &self.models), (effort, effort_choices)]
                .into_iter()
                .any(|(value, choices)| {
                    value.is_some_and(|value| {
                        choices
                            .as_ref()
                            .is_some_and(|choices| !choices.iter().any(|c| c == value))
                    })
                });
        // Invalid input is omitted, so an attempted command injection or secret
        // pasted as a value does not become part of retained observations.
        self.requested = Some(json!({
            "model": model.filter(|s| identifier(s)), "effort": effort.filter(|s| identifier(s)),
            "requested_at_ms": now, "finished_at_ms": now,
            "state": if invalid_syntax || invalid_choice { "failed" } else { "unsupported" },
            "validation": if invalid_syntax { "invalid_identifier" } else if invalid_choice { "not_in_advertised_options" } else if fresh_options { "advertised_options_where_available" } else { "syntax_only_options_unavailable" },
            "reason": if invalid_syntax || invalid_choice { "Requested value failed validation; no input or control request was sent." } else { UNSUPPORTED },
            "pending": false, "sent_to_harness": false, "applied_acknowledgement": null,
        }));
    }

    pub fn snapshot(&self, now: u64) -> Value {
        let age = self
            .evidence
            .as_ref()
            .map(|e| now.saturating_sub(e.observed_at_ms));
        let stale = self.invalidated || age.is_none_or(|age| age > STALE_AFTER_MS);
        let options_stale = self.invalidated
            || self
                .options_evidence
                .as_ref()
                .is_none_or(|e| now.saturating_sub(e.observed_at_ms) > STALE_AFTER_MS);
        json!({
            "control": {"state": "unsupported", "reason": UNSUPPORTED},
            "requested": self.requested,
            "applied": {"model": if stale { None } else { self.model.as_ref() },
                "effort": if stale { None } else { self.effort.as_ref() },
                "last_observed_model": self.model, "last_observed_effort": self.effort,
                "evidence": self.evidence, "age_ms": age, "stale": stale,
                "in_flight_inference_configuration": "unknown"},
            "options": {"models": if options_stale { None } else { self.models.as_ref() },
                "efforts": if options_stale { None } else { self.efforts.as_ref() },
                "evidence": self.options_evidence, "stale": options_stale,
                "availability": if options_stale { "unknown" } else { "advertised_by_owning_acp_client" }},
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_values_are_separate_from_observed_applied_configuration() {
        let mut c = Configuration::default();
        c.observe_status_line(&json!({"model_id":"old", "effort":"low"}), 1);
        c.request(Some("gpt-6.1-sol"), Some("high"), 2);
        let v = c.snapshot(3);
        assert_eq!(v["requested"]["state"], "unsupported");
        assert_eq!(v["requested"]["model"], "gpt-6.1-sol");
        assert_eq!(v["requested"]["effort"], "high");
        assert_eq!(v["requested"]["sent_to_harness"], false);
        assert!(v["requested"]["applied_acknowledgement"].is_null());
        assert_eq!(v["applied"]["model"], "old");
        c.observe_status_line(&json!({"model_id":"gpt-6.1-sol", "effort":"high"}), 4);
        assert_eq!(c.snapshot(5)["applied"]["model"], "gpt-6.1-sol");
        assert_eq!(c.snapshot(5)["requested"]["state"], "unsupported");
        assert!(c.snapshot(STALE_AFTER_MS + 5)["applied"]["model"].is_null());
    }

    #[test]
    fn effort_choices_follow_the_observed_model() {
        let mut c = Configuration::default();
        c.observe_options(&json!({"config_options":[
            {"id":"model", "type":"select", "currentValue":"old", "options":[{"value":"old"}, {"value":"new"}]},
            {"id":"reasoning_effort", "type":"select", "currentValue":"low", "options":[{"value":"low"}]}
        ]}), 1).unwrap();
        c.request(Some("new"), Some("high"), 2);
        assert_eq!(c.snapshot(3)["requested"]["state"], "unsupported");
        c.request(Some("old"), Some("high"), 4);
        assert_eq!(c.snapshot(5)["requested"]["state"], "failed");
        c.observe_status_line(&json!({"model_id":"new", "effort":"high"}), 6);
        let v = c.snapshot(7);
        assert_eq!(v["applied"]["model"], "new");
        assert!(v["options"]["efforts"].is_null());
        c.request(None, Some("high"), 8);
        assert_eq!(c.snapshot(9)["requested"]["state"], "unsupported");
    }

    #[test]
    fn menus_validate_exact_model_specific_values_including_grouped_options() {
        let mut c = Configuration::default();
        c.observe_options(&json!({"config_options":[
            {"id":"model", "type":"select", "currentValue":"gpt-6.1-sol", "options":[{"name":"Models", "options":[{"value":"gpt-6.1-sol"}]}]},
            {"id":"reasoning_effort", "type":"select", "currentValue":"high", "options":[{"value":"high"}, {"value":"deep"}]}
        ]}), 1).unwrap();
        c.request(Some("gpt-6.1-sol"), Some("deep"), 2);
        assert_eq!(c.snapshot(3)["requested"]["state"], "unsupported");
        assert_eq!(c.snapshot(3)["options"]["efforts"], json!(["high", "deep"]));
        c.request(Some("invented"), Some("max"), 4);
        assert_eq!(c.snapshot(5)["requested"]["state"], "failed");
        assert_eq!(
            c.snapshot(5)["requested"]["validation"],
            "not_in_advertised_options"
        );
        c.request(Some("gpt-6.1-sol; rm -rf /"), None, 6);
        assert_eq!(c.snapshot(7)["requested"]["state"], "failed");
        assert!(c.snapshot(7)["requested"]["model"].is_null());
        c.request(Some("new-model"), None, STALE_AFTER_MS + 2);
        assert_eq!(
            c.snapshot(STALE_AFTER_MS + 3)["requested"]["validation"],
            "syntax_only_options_unavailable"
        );
    }
}
