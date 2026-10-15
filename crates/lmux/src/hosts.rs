//! Explicit, host-scoped CLI routing over the user's SSH transport.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context};
use futures_util::{stream, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct Host {
    pub name: String,
    pub label: String,
    pub transport: String,
    pub ssh_target: Option<String>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Endpoint {
    Local(String),
    Ssh {
        name: String,
        target: String,
        socket: Option<String>,
    },
}

fn valid_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 255
        && !target.starts_with('-')
        && target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@:%[]".contains(&b))
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Endpoint {
    pub fn select(
        host: Option<&str>,
        socket: Option<&str>,
        local_socket: &str,
        hosts: &[Host],
    ) -> anyhow::Result<Self> {
        let Some(name) = host.filter(|name| *name != "local") else {
            return Ok(Self::Local(socket.unwrap_or(local_socket).into()));
        };
        let entry = match hosts.iter().find(|h| h.name == name) {
            Some(entry) => Some(entry),
            None => {
                let matches: Vec<_> = hosts
                    .iter()
                    .filter(|h| h.label == name || h.ssh_target.as_deref() == Some(name))
                    .collect();
                match matches.as_slice() {
                    [] => None,
                    [entry] => Some(*entry),
                    _ => bail!("host label {name:?} matches several routes; select a name from `lmux list-hosts`"),
                }
            }
        };
        if entry.is_some_and(|h| h.transport != "ssh") {
            bail!("host {name:?} uses an app-only transport; select its SSH/Tailscale alias with --host");
        }
        let target = entry.and_then(|h| h.ssh_target.as_deref()).unwrap_or(name);
        if !valid_target(target) {
            bail!(
                "invalid SSH target {target:?}; use an SSH alias, user@host, or Tailscale address"
            );
        }
        Ok(Self::Ssh {
            name: name.into(),
            target: target.into(),
            socket: socket.map(String::from),
        })
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Ssh { .. })
    }

    pub async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        match self {
            Self::Local(socket) => crate::call(socket, method, params).await.map_err(|error| {
                if error.to_string().contains("(NotFound)") {
                    error.context(format!("target lookup failed on the selected daemon ({socket}); refs are host-scoped. Use `lmux list-hosts` and `lmux --host HOST tree` to locate another host"))
                } else {
                    error
                }
            }),
            Self::Ssh { name, target, socket } => {
                let mut value = remote_call(target, socket.as_deref(), method, params)
                    .await.with_context(|| format!("host {name} (SSH {target}); refs belong to this host"))?;
                if let Some(object) = value.as_object_mut() {
                    object.insert("cli_target".into(), json!({"host":name,"transport":"ssh","ssh_target":target}));
                }
                Ok(value)
            }
        }
    }
}

fn remote_command(socket: Option<&str>, method: &str) -> String {
    let socket = socket
        .map(|s| format!(" --socket {}", quote(s)))
        .unwrap_or_default();
    // JSON travels on stdin. Shell expansion cannot interpret its contents.
    // Clearing caller context also guards SSH SendEnv/AcceptEnv configurations.
    format!("unset LMUX_SOCKET LMUX_STATE_DIR LMUX_WORKSPACE_ID LMUX_SURFACE_ID; export PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\"; exec lmux{socket} --json rpc {} \"$(cat)\"", quote(method))
}

async fn remote_call(
    target: &str,
    socket: Option<&str>,
    method: &str,
    mut params: Value,
) -> anyhow::Result<Value> {
    if let Some(params) = params.as_object_mut() {
        for key in ["caller_workspace", "caller_surface", "caller_pid"] {
            params.remove(key);
        }
    }
    let input = serde_json::to_vec(&params)?;
    // Older remote CLIs accept JSON as one argv value. Stay below Linux's
    // per-argument limit until a negotiated stdin RPC protocol is available.
    if input.len() > 64 * 1024 {
        bail!("remote RPC request exceeds 64 KiB; no SSH request was sent. Use a local CLI for larger pastes");
    }
    let limit = request_timeout(method, &params);
    let operation = async {
        let mut command = Command::new("ssh");
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("LMUX_"))
        {
            command.env_remove(key);
        }
        let mut child = command
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
            ])
            .arg("--")
            .arg(target)
            .arg(remote_command(socket, method))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting SSH")?;
        let mut stdin = child.stdin.take().context("SSH stdin unavailable")?;
        let write = async move {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        };
        let (written, output) = tokio::join!(write, child.wait_with_output());
        let output = output.context("waiting for SSH")?;
        if !output.status.success() {
            bail!(
                "SSH/lmux failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        written.context("sending request to SSH")?;
        serde_json::from_slice(&output.stdout).context("remote lmux returned invalid JSON")
    };
    tokio::time::timeout(limit, operation).await
        .context("SSH request timed out; the remote operation's outcome is unknown. Inspect the selected host before retrying a change")?
}

fn request_timeout(method: &str, params: &Value) -> Duration {
    let overhead = Duration::from_secs(20);
    if method == lmux_proto::methods::BROWSER_WAIT {
        overhead + Duration::from_millis(params["timeout_ms"].as_u64().unwrap_or(5000))
    } else {
        overhead
    }
}

pub async fn call(endpoint: &Endpoint, method: &str, params: Value) -> anyhow::Result<Value> {
    endpoint.call(method, params).await
}

pub fn discover() -> anyhow::Result<Vec<Host>> {
    let home = crate::dirs_home().context("HOME is unavailable")?;
    let path = crate::cp::hosts_file();
    let registry = match path.as_ref().map(std::fs::read_to_string) {
        Some(Ok(text)) => serde_json::from_str(&text).context("reading lmux hosts.json")?,
        Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).context("reading lmux hosts.json")
        }
        _ => Value::Null,
    };
    let ssh_path = home.join(".ssh/config");
    let ssh_config = match std::fs::read_to_string(&ssh_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context("reading ~/.ssh/config"),
    };
    discover_from(&registry, path.as_deref(), &ssh_config, &ssh_path)
}

fn discover_from(
    registry: &Value,
    registry_path: Option<&Path>,
    ssh: &str,
    ssh_path: &Path,
) -> anyhow::Result<Vec<Host>> {
    if !registry.is_null() && !registry.is_array() {
        bail!("lmux hosts.json must contain a host array");
    }
    let mut hosts = vec![Host {
        name: "local".into(),
        label: "this machine".into(),
        transport: "local".into(),
        ssh_target: None,
        sources: vec!["local daemon socket".into()],
    }];
    for entry in registry.as_array().into_iter().flatten() {
        let name = entry["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("hosts.json entry requires a name")?;
        if name == "local" || hosts.iter().any(|h| h.name == name) {
            bail!("duplicate or reserved host name {name:?} in hosts.json");
        }
        let target = entry["ssh_target"].as_str().map(String::from);
        let transport = entry["kind"].as_str().unwrap_or("unknown");
        if transport == "ssh" && !target.as_deref().is_some_and(valid_target) {
            bail!("host {name:?} requires a valid ssh_target");
        }
        hosts.push(Host {
            name: name.into(),
            label: entry["label"].as_str().unwrap_or(name).into(),
            transport: transport.into(),
            ssh_target: target,
            sources: vec![registry_path
                .unwrap_or(Path::new("hosts.json"))
                .display()
                .to_string()],
        });
    }
    // Literal aliases only. OpenSSH resolves Include, Match and wildcard rules
    // when a host is explicitly selected; discovery performs no network scan.
    for line in ssh.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut words = line.split_whitespace();
        if !words.next().is_some_and(|s| s.eq_ignore_ascii_case("host")) {
            continue;
        }
        for alias in words.filter(|s| valid_target(s) && *s != "local") {
            if let Some(entry) = hosts
                .iter_mut()
                .find(|h| h.name == alias || h.ssh_target.as_deref() == Some(alias))
            {
                let source = ssh_path.display().to_string();
                if !entry.sources.contains(&source) {
                    entry.sources.push(source);
                }
                continue;
            }
            hosts.push(Host {
                name: alias.into(),
                label: alias.into(),
                transport: "ssh".into(),
                ssh_target: Some(alias.into()),
                sources: vec![ssh_path.display().to_string()],
            });
        }
    }
    Ok(hosts)
}

pub fn print_hosts(hosts: &[Host], json_output: bool) {
    if json_output {
        println!("{}", serde_json::to_string_pretty(&json!({"hosts":hosts,"scope":"configured_routes","discovery":"app hosts.json and literal ~/.ssh/config Host aliases; no network scan"})).unwrap());
    } else {
        for host in hosts {
            println!(
                "{}\t{}\t{}",
                host.name,
                host.transport,
                host.ssh_target.as_deref().unwrap_or("this machine")
            );
        }
        println!(
            "Use `lmux --host HOST tree`. Refs belong to that host. SSH aliases can use Tailscale."
        );
    }
}

pub async fn all_trees(
    hosts: &[Host],
    local_socket: &str,
    json_output: bool,
) -> anyhow::Result<()> {
    if hosts.len() > 256 {
        bail!("aggregate discovery exceeds 256 entries; select an explicit --host");
    }
    let results: Vec<Value> = stream::iter(hosts.iter().map(|host| async move {
        let endpoint = Endpoint::select(Some(&host.name), None, local_socket, hosts);
        match endpoint {
            Ok(endpoint) => match tokio::time::timeout(Duration::from_secs(20), endpoint.call(lmux_proto::methods::TREE, json!({}))).await {
                Ok(Ok(tree)) if tree["workspaces"].is_array() => json!({"name":host.name,"state":"connected","transport":host.transport,"tree":tree}),
                Ok(Ok(_)) => json!({"name":host.name,"state":"unreachable","transport":host.transport,"error":"daemon returned an invalid tree","tree":null}),
                Ok(Err(error)) => json!({"name":host.name,"state":"unreachable","transport":host.transport,"error":format!("{error:#}"),"tree":null}),
                Err(_) => json!({"name":host.name,"state":"unreachable","transport":host.transport,"error":"tree query timed out after 20 seconds","tree":null}),
            },
            Err(error) => json!({"name":host.name,"state":"unsupported","transport":host.transport,"error":format!("{error:#}"),"tree":null}),
        }
    })).buffered(8).collect().await;
    let partial = results.iter().any(|entry| entry["state"] != "connected");
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"scope":"all_hosts","hosts":results,"partial":partial})
            )
            .unwrap()
        );
    } else {
        for result in &results {
            println!(
                "target {} [{}]",
                result["name"].as_str().unwrap_or("?"),
                result["state"].as_str().unwrap_or("unknown")
            );
            if result["state"] == "connected" {
                crate::commands::print_tree(&result["tree"]);
            } else {
                println!("  {}", result["error"].as_str().unwrap_or("unknown error"));
            }
        }
    }
    if partial {
        bail!("some hosts could not be queried; the tree is partial (see each host's status)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_app_hosts_and_literal_ssh_aliases_without_network_scanning() {
        let hosts = discover_from(&json!([
            {"name":"build", "label":"Build Mac", "kind":"ssh", "ssh_target":"lint-hqc-03"},
            {"name":"preview", "kind":"direct", "url":"ws://localhost:7900"}
        ]), Some(Path::new("/app/hosts.json")), "Host lint-hqc-03 another\nHost *.private !excluded\n hOsT lint-hqc-01 # comment\nInclude conf/*", Path::new("/home/.ssh/config")).unwrap();
        assert_eq!(
            hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            ["local", "build", "preview", "another", "lint-hqc-01"]
        );
        assert_eq!(hosts[1].sources.len(), 2);
        assert!(
            matches!(Endpoint::select(Some("Build Mac"), None, "/local", &hosts).unwrap(), Endpoint::Ssh { target, .. } if target == "lint-hqc-03")
        );
        assert!(Endpoint::select(Some("preview"), None, "/local", &hosts).is_err());
    }

    #[test]
    fn explicit_hosts_validate_targets_and_keep_socket_scopes_separate() {
        for target in [
            "-oProxyCommand=bad",
            "host;touch x",
            "host\nother",
            "",
            "host name",
        ] {
            assert!(Endpoint::select(Some(target), None, "/local", &[]).is_err());
        }
        for target in ["lint-hqc-01", "user@box", "100.64.0.3", "fd7a:115c:a1e0::1"] {
            assert!(Endpoint::select(Some(target), None, "/local", &[])
                .unwrap()
                .is_remote());
        }
        assert!(matches!(
            Endpoint::select(Some("box"), None, "/local", &[]).unwrap(),
            Endpoint::Ssh { socket: None, .. }
        ));
        assert!(
            matches!(Endpoint::select(Some("box"), Some("/remote"), "/local", &[]).unwrap(), Endpoint::Ssh { socket: Some(s), .. } if s == "/remote")
        );
    }

    #[test]
    fn large_discovery_keeps_explicit_routing_and_rejects_ambiguous_labels() {
        let aliases = (0..300)
            .map(|i| format!("Host host-{i}\n"))
            .collect::<String>();
        let hosts = discover_from(&Value::Null, None, &aliases, Path::new("/ssh/config")).unwrap();
        assert_eq!(hosts.len(), 301);
        assert!(Endpoint::select(Some("host-299"), None, "/local", &hosts).is_ok());
        let ambiguous = discover_from(
            &json!([
                {"name":"first","label":"same","kind":"ssh","ssh_target":"a"},
                {"name":"second","label":"same","kind":"ssh","ssh_target":"b"}
            ]),
            None,
            "",
            Path::new("/ssh/config"),
        )
        .unwrap();
        assert!(Endpoint::select(Some("same"), None, "/local", &ambiguous).is_err());
        assert!(discover_from(&json!({}), None, "", Path::new("/ssh/config")).is_err());
        assert!(discover_from(
            &json!([{"name":"local"}]),
            None,
            "",
            Path::new("/ssh/config")
        )
        .is_err());
    }

    #[test]
    fn browser_wait_transport_allows_the_requested_wait_plus_ssh_overhead() {
        assert_eq!(
            request_timeout(
                lmux_proto::methods::BROWSER_WAIT,
                &json!({"timeout_ms":60000})
            ),
            Duration::from_secs(80)
        );
        assert_eq!(
            request_timeout(lmux_proto::methods::TREE, &json!({})),
            Duration::from_secs(20)
        );
    }

    #[test]
    fn remote_shell_quotes_only_control_fields_and_clears_caller_context() {
        let command = remote_command(Some("/tmp/a'b"), "system.tree' ; false");
        assert!(command.contains("--socket '/tmp/a'\\''b'"));
        assert!(command.contains("rpc 'system.tree'\\'' ; false'"));
        assert!(
            command.contains("unset LMUX_SOCKET LMUX_STATE_DIR LMUX_WORKSPACE_ID LMUX_SURFACE_ID")
        );
        assert!(command.ends_with("\"$(cat)\""));
    }
}
