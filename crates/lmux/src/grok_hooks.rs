//! Opt-in supported Grok hooks. Never emit a hook decision or copy tool data.

use std::io::Read;

use lmux_core::harness::grok_envelope;
use lmux_proto::methods;
use serde_json::{json, Value};

use crate::cli::GrokHook;

pub fn config() -> Value {
    let mut hooks = serde_json::Map::new();
    for event in [
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "PreCompact",
        "PostCompact",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "Stop",
        "StopFailure",
        "StopCancelled",
        "Notification",
    ] {
        hooks.insert(event.into(), json!([{"hooks": [{"type": "command",
            "command": "if [ -n \"$LMUX_SURFACE_ID\" ]; then lmux hooks grok event >/dev/null 2>/dev/null; fi; true",
            "timeout": 2}]}]));
    }
    json!({"hooks": hooks})
}

pub async fn run(cmd: GrokHook, socket: &str) -> anyhow::Result<()> {
    if matches!(cmd, GrokHook::PrintConfig) {
        println!("{}", serde_json::to_string_pretty(&config())?);
        return Ok(());
    }
    // In particular a background subagent must never update its parent's tab.
    if std::env::var("LMUX_SURFACE_ID")
        .ok()
        .is_none_or(|s| s.is_empty())
    {
        return Ok(());
    }
    let mut input = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut input)?;
    let payload: Value = serde_json::from_str(&input)?;
    let mut envelope = grok_envelope(&payload, matches!(cmd, GrokHook::StatusLine));
    envelope["surface"] = json!(std::env::var("LMUX_SURFACE_ID")?);
    if envelope["subagent"] == true {
        return Ok(());
    }
    // Silence and fail-open are part of the observation hook contract.
    crate::call_quick(
        socket,
        methods::HARNESS_OBSERVE,
        envelope,
        std::time::Duration::from_millis(750),
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_passive_and_has_short_timeouts() {
        let config = config();
        assert!(config["hooks"]["PreCompact"].is_array());
        for groups in config["hooks"].as_object().unwrap().values() {
            let handler = &groups[0]["hooks"][0];
            assert_eq!(handler["timeout"], 2);
            assert!(handler["command"].as_str().unwrap().ends_with("true"));
        }
    }
}
