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

/// Whether `error` is the daemon saying the folder named `folder` is not there.
pub fn folder_missing(error: &anyhow::Error, folder: &str) -> bool {
    format!("{error:#}").contains(&format!("folder {folder} not found"))
}

/// The one SSH host whose daemon has a folder titled `title`, for commands that
/// name a folder without --host. Refs are host-scoped, so only titles qualify.
pub async fn host_with_folder(title: &str) -> anyhow::Result<Endpoint> {
    if title.starts_with("folder:") || title.bytes().all(|c| c.is_ascii_digit()) {
        bail!("folder refs belong to one host; add --host HOST");
    }
    let hosts = discover()?;
    let remote: Vec<&Host> = hosts.iter().filter(|h| h.transport == "ssh").collect();
    if remote.len() > 64 {
        bail!("too many hosts to search for folder {title:?}; add --host HOST");
    }
    let found: Vec<(String, Result<bool, String>)> = stream::iter(remote.iter().map(|host| {
        let hosts = &hosts;
        async move {
            let result = async {
                let endpoint = Endpoint::select(Some(&host.name), None, "", hosts).map_err(|e| format!("{e:#}"))?;
                let tree = tokio::time::timeout(Duration::from_secs(20), endpoint.call(lmux_proto::methods::TREE, json!({})))
                    .await
                    .map_err(|_| "timed out".to_string())?
                    .map_err(|e| format!("{e:#}"))?;
                Ok(tree["folders"].as_array().into_iter().flatten().any(|f| f["name"] == title))
            }
            .await;
            (host.name.clone(), result)
        }
    }))
    .buffered(8)
    .collect()
    .await;
    pick_folder_host(title, &found).and_then(|name| Endpoint::select(Some(&name), None, "", &hosts))
}

/// Exactly one host with the folder wins; none or several is an error that
/// says which hosts could not be asked.
fn pick_folder_host(title: &str, found: &[(String, Result<bool, String>)]) -> anyhow::Result<String> {
    let with: Vec<&str> = found.iter().filter(|(_, r)| r == &Ok(true)).map(|(n, _)| n.as_str()).collect();
    let unreachable: Vec<&str> = found.iter().filter(|(_, r)| r.is_err()).map(|(n, _)| n.as_str()).collect();
    match with.as_slice() {
        [one] => {
            eprintln!("lmux: folder {title:?} is on host {one}");
            Ok(one.to_string())
        }
        [] if unreachable.is_empty() => bail!("no host has a folder named {title:?}"),
        [] => bail!("no reachable host has a folder named {title:?} (not asked: {})", unreachable.join(", ")),
        many => bail!("folder {title:?} exists on several hosts ({}); add --host HOST", many.join(", ")),
    }
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
    // Git hosting aliases are SSH hosts but never run lmux.
    for line in ssh.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut words = line.split_whitespace();
        if !words.next().is_some_and(|s| s.eq_ignore_ascii_case("host")) {
            continue;
        }
        for alias in words.filter(|s| valid_target(s) && *s != "local" && !GIT_HOSTS.contains(&s.to_ascii_lowercase().as_str())) {
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

const GIT_HOSTS: &[&str] = &["github.com", "gitlab.com", "bitbucket.org", "ssh.dev.azure.com", "codeberg.org"];

/// How an SSH route reaches its host, from the address OpenSSH would dial.
pub fn route_kind(address: &str, magic_suffix: Option<&str>) -> &'static str {
    let a = address.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    if let Ok(ip) = a.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) if crate::share::tailnet::in_tailnet_range(&v4) => "tailnet",
            std::net::IpAddr::V4(v4) if v4.is_loopback() => "loopback",
            std::net::IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => "lan",
            std::net::IpAddr::V6(v6) if v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0] => "tailnet",
            std::net::IpAddr::V6(v6) if v6.is_loopback() => "loopback",
            std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80 => "lan",
            _ => "public",
        };
    }
    if a.ends_with(".ts.net") || magic_suffix.is_some_and(|s| a.ends_with(&format!(".{s}"))) {
        "tailnet"
    } else if a.ends_with(".local") || !a.contains('.') {
        "lan"
    } else {
        "name"
    }
}

/// The address OpenSSH would dial for `alias` (`ssh -G`: reads the config, no network).
async fn ssh_address(alias: &str) -> Option<String> {
    let out = tokio::time::timeout(Duration::from_secs(5), Command::new("ssh").args(["-G", "--", alias]).stdin(Stdio::null()).stderr(Stdio::null()).output())
        .await
        .ok()?
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix("hostname ").map(str::to_owned))
}

/// Lists the routes with the address each one dials and how it gets there
/// (`tailnet`, `lan`, `public`, `name`), so a route that bypasses the tailnet
/// shows up.
pub async fn print_hosts(hosts: &[Host], json_output: bool) {
    let magic = crate::share::tailnet::detect().await.and_then(|t| t.dns).and_then(|d| d.split_once('.').map(|(_, s)| s.to_string()));
    let routes: Vec<(Option<String>, Option<&'static str>)> = stream::iter(hosts.iter().map(|h| {
        let magic = magic.clone();
        async move {
            let target = h.ssh_target.as_deref().filter(|_| h.transport == "ssh")?;
            let address = ssh_address(target).await?;
            let kind = route_kind(&address, magic.as_deref());
            Some((Some(address), Some(kind)))
        }
    }))
    .buffered(16)
    .map(|r| r.unwrap_or((None, None)))
    .collect()
    .await;
    if json_output {
        let list: Vec<Value> = hosts.iter().zip(&routes).map(|(h, (address, kind))| {
            let mut v = serde_json::to_value(h).unwrap_or(Value::Null);
            v["address"] = json!(address);
            v["route"] = json!(kind);
            v
        }).collect();
        println!("{}", serde_json::to_string_pretty(&json!({"hosts":list,"scope":"configured_routes","discovery":"app hosts.json and literal ~/.ssh/config Host aliases; no network scan"})).unwrap());
    } else {
        for (host, (address, kind)) in hosts.iter().zip(&routes) {
            let via = match (address, kind) {
                (Some(a), Some(k)) => format!("{a} ({k})"),
                _ => String::new(),
            };
            println!(
                "{}\t{}\t{}\t{}",
                host.name,
                host.transport,
                host.ssh_target.as_deref().unwrap_or("this machine"),
                via
            );
        }
        let off: Vec<&str> = hosts.iter().zip(&routes).filter(|(_, (_, k))| matches!(k, Some("public" | "lan"))).map(|(h, _)| h.name.as_str()).collect();
        if !off.is_empty() {
            println!("Not over the tailnet: {}. Point their HostName at the tailnet address when the host has one.", off.join(", "));
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
    fn a_folder_title_routes_to_the_one_host_that_has_it() {
        let found = |v: &[(&str, Result<bool, &str>)]| {
            v.iter().map(|(n, r)| (n.to_string(), r.map_err(String::from))).collect::<Vec<_>>()
        };
        assert_eq!(
            pick_folder_host("justcallme", &found(&[("a", Ok(false)), ("b", Ok(true)), ("c", Err("down"))])).unwrap(),
            "b"
        );
        let none = pick_folder_host("x", &found(&[("a", Ok(false)), ("c", Err("down"))])).unwrap_err();
        assert!(none.to_string().contains("not asked: c"), "{none}");
        let many = pick_folder_host("x", &found(&[("a", Ok(true)), ("b", Ok(true))])).unwrap_err();
        assert!(many.to_string().contains("several hosts (a, b)"), "{many}");
        assert!(folder_missing(&anyhow::anyhow!("rpc error (NotFound): folder justcallme not found"), "justcallme"));
        assert!(!folder_missing(&anyhow::anyhow!("rpc error (NotFound): folder other not found"), "justcallme"));
    }

    #[test]
    fn routes_are_classified_by_the_address_they_dial() {
        let m = Some("hs.lint.internal");
        assert_eq!(route_kind("100.64.0.15", m), "tailnet");
        assert_eq!(route_kind("fd7a:115c:a1e0::2", m), "tailnet");
        assert_eq!(route_kind("lint-hqc-01.hs.lint.internal", m), "tailnet");
        assert_eq!(route_kind("box.tail1234.ts.net", None), "tailnet");
        assert_eq!(route_kind("192.168.0.8", m), "lan");
        assert_eq!(route_kind("RENU.local", m), "lan");
        assert_eq!(route_kind("109.123.251.196", m), "public");
        assert_eq!(route_kind("100.128.0.1", m), "public", "outside 100.64/10");
        assert_eq!(route_kind("example.com", m), "name");
        let hosts = discover_from(&Value::Null, None, "Host github.com box\nHost GitLab.com\n", Path::new("/c")).unwrap();
        assert_eq!(hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["local", "box"], "git hosts are not lmux hosts");
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
