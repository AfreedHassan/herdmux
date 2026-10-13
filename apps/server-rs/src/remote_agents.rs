//! Best-effort discovery of agents running on SSH hosts listed in
//! `~/.config/herdmux/remotes.json`. Each host's `~/.ai/sessions`
//! bindings are read over a short, non-interactive SSH call; live agents are
//! attached to the local tmux pane running `ssh <host>`. Unreachable hosts are
//! skipped silently: no prompts, no retries beyond the next poll.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use herdmux_runtime::protocol::{AgentEvent, AgentLiveness, AgentStatus};
use serde_json::Value;

pub(crate) const REMOTE_POLL_SECS: u64 = 3;
const SSH_TIMEOUT: Duration = Duration::from_secs(6);

/// Runs on the remote host. Agents are found from the remote tmux panes
/// (agent process under the pane), then enriched with the newest
/// `~/.ai/sessions` binding for that pane (status, title, thread id).
const REMOTE_SCRIPT: &str = r##"
import datetime, glob, json, os, re, subprocess
def run(*args):
    try:
        return subprocess.run(list(args), capture_output=True, text=True, timeout=3).stdout
    except Exception:
        return ""
def updated_ms(record):
    try:
        return int(datetime.datetime.fromisoformat(str(record.get("updated_at"))).timestamp() * 1000)
    except Exception:
        return 0
def agent_for(comm):
    name = os.path.basename(comm)
    if name == "codex":
        return "codex"
    if name == "claude" or re.fullmatch(r"\d+\.\d+\.\d+", name):
        return "claude"
    return None
children, agents = {}, {}
for line in run("ps", "-eo", "pid=,ppid=,comm=").splitlines():
    parts = line.split(None, 2)
    if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
        children.setdefault(int(parts[1]), []).append(int(parts[0]))
        tool = agent_for(parts[2].strip())
        if tool:
            agents[int(parts[0])] = tool
def agent_under(pid):
    stack, seen = [pid], set()
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        if current in agents:
            return agents[current]
        stack.extend(children.get(current, []))
    return None
latest = {}
for path in glob.glob(os.path.expanduser("~/.ai/sessions/*.json")):
    try:
        record = json.load(open(path))
    except Exception:
        continue
    key = (record.get("tool"), record.get("pane"))
    if record.get("status") != "ended" and (key not in latest or str(record.get("updated_at")) > str(latest[key].get("updated_at"))):
        latest[key] = record
def native_title(record):
    """Latest /rename (custom-title) or automatic title, like local names."""
    if record.get("tool") == "codex":
        return codex_titles.get(record.get("id"))
    path = record.get("transcript")
    if not isinstance(path, str):
        return None
    try:
        with open(path, "rb") as handle:
            handle.seek(0, 2)
            size = handle.tell()
            handle.seek(max(0, size - 262144))
            tail = handle.read().decode("utf-8", "replace")
    except Exception:
        return None
    custom = automatic = None
    for line in tail.splitlines():
        if '"custom-title"' not in line and '"ai-title"' not in line:
            continue
        try:
            entry = json.loads(line)
        except Exception:
            continue
        if entry.get("type") == "custom-title" and entry.get("customTitle"):
            custom = entry["customTitle"]
        elif entry.get("type") == "ai-title" and entry.get("aiTitle"):
            automatic = entry["aiTitle"]
    return custom or automatic
codex_titles = {}
try:
    for line in open(os.path.expanduser("~/.codex/session_index.jsonl")):
        try:
            entry = json.loads(line)
            if entry.get("id") and entry.get("thread_name"):
                codex_titles[entry["id"]] = entry["thread_name"]
        except Exception:
            pass
except Exception:
    pass
fmt = "#{pane_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{pane_pid}\t#{&&:#{session_attached},#{&&:#{window_active},#{pane_active}}}\t#{@herdmux_state}"
for line in run("tmux", "list-panes", "-a", "-F", fmt).splitlines():
    parts = line.split("\t")
    if len(parts) != 7 or not parts[4].isdigit():
        continue
    pane, session, window_index, window_name, pid, active, hook_state = parts
    # Drop status glyphs that agent indicators append to window names.
    window_name = re.sub(r"\s+[^\w\s-]+$", "", window_name)
    tool = agent_under(int(pid))
    if not tool:
        continue
    record = latest.get((tool, pane), {})
    # Hook-set pane state (herdmux-agent-state) wins over the binding.
    status = {"needs-input": "needs_input", "running": "working", "done": "done"}.get(hook_state, record.get("status"))
    print(json.dumps({"tool": tool, "pane": pane, "session": session,
                      "window": window_index + ":" + window_name,
                      "id": record.get("id"), "status": status,
                      "title": native_title(record) or record.get("title"),
                      "updated_ms": updated_ms(record),
                      "active": active == "1"}))
"##;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteBinding {
    pub tool: String,
    pub pane: String,
    pub id: Option<String>,
    pub status: Option<String>,
    pub title: Option<String>,
    pub session: Option<String>,
    pub window: Option<String>,
    pub updated_ms: u64,
    /// The pane the host's attached client is looking at.
    pub active: bool,
}

impl RemoteBinding {
    fn from_value(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            tool: text("tool")?,
            pane: text("pane")?,
            id: text("id"),
            status: text("status"),
            title: text("title"),
            session: text("session"),
            window: text("window"),
            updated_ms: value.get("updated_ms").and_then(Value::as_u64).unwrap_or(0),
            active: value.get("active").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalSshPane {
    pub pane_id: String,
    pub session: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RemoteInventory {
    /// Local session name -> agents shown in it.
    pub agents_by_session: HashMap<String, Vec<AgentEvent>>,
    /// Local panes that proxy a remote host (never kill these as agents).
    pub proxy_panes: HashSet<String>,
    /// Row thread id -> (host, remote tmux pane) for jumping inside the host.
    pub remote_targets: HashMap<String, (String, String)>,
    /// Local proxy pane -> thread id of the agent active on the host.
    pub active_thread_by_proxy: HashMap<String, String>,
}

pub(crate) fn load_hosts(home: &Path) -> Vec<String> {
    std::fs::read_to_string(home.join(".config/herdmux/remotes.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|config| config.get("hosts").and_then(Value::as_array).cloned())
        .map(|hosts| {
            hosts
                .iter()
                .filter_map(Value::as_str)
                .filter(|host| valid_host(host))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(crate) fn parse_bindings(raw: &str) -> Vec<RemoteBinding> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| RemoteBinding::from_value(&value))
        .collect()
}

/// Fetch one host's live bindings; `None` when unreachable or too slow.
/// Non-interactive ssh sharing one persistent connection per host, so
/// polls and jumps skip the handshake.
fn ssh(host: &str) -> Command {
    let mut command = Command::new("ssh");
    command.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=3",
        "-o",
        "ServerAliveInterval=2",
        "-o",
        "ServerAliveCountMax=1",
        "-o",
        "ControlMaster=auto",
        "-o",
        "ControlPath=/tmp/herdmux-ssh-%C",
        "-o",
        "ControlPersist=120",
        "--",
        host,
    ]);
    command
}

/// Select the agent's window and pane inside the host's tmux (best effort).
pub(crate) fn select_remote_pane(host: &str, pane: &str) {
    if !valid_host(host) || !pane.starts_with('%') || !pane[1..].bytes().all(|b| b.is_ascii_digit()) {
        return;
    }
    let host = host.to_string();
    let pane = pane.to_string();
    std::thread::spawn(move || {
        let Ok(mut child) = ssh(&host)
            .args(["tmux", "select-window", "-t", &pane, "\\;", "select-pane", "-t", &pane])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let deadline = Instant::now() + SSH_TIMEOUT;
        while Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    });
}

pub(crate) fn fetch_host(host: &str) -> Option<Vec<RemoteBinding>> {
    let mut child = ssh(host)
        .args(["python3", "-c", &shell_quote(REMOTE_SCRIPT)])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + SSH_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut raw = String::new();
    child.stdout.take()?.read_to_string(&mut raw).ok()?;
    Some(parse_bindings(&raw))
}

/// tty -> host for local `ssh <host>` processes, from one `ps` call.
pub(crate) fn local_ssh_ttys(hosts: &[String]) -> HashMap<String, String> {
    let Ok(output) = Command::new("ps")
        .args(["-ax", "-o", "tty=,args="])
        .output()
    else {
        return HashMap::new();
    };
    parse_ssh_ttys(&String::from_utf8_lossy(&output.stdout), hosts)
}

pub(crate) fn parse_ssh_ttys(raw: &str, hosts: &[String]) -> HashMap<String, String> {
    let mut ttys = HashMap::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let (Some(tty), Some(command)) = (fields.next(), fields.next()) else {
            continue;
        };
        if tty == "??" || Path::new(command).file_name().and_then(|name| name.to_str()) != Some("ssh")
        {
            continue;
        }
        // Interactive sessions only: the first non-option argument is the host
        // and nothing follows it (a remote command would be a one-shot call).
        let args = fields.collect::<Vec<_>>();
        let Some(position) = args.iter().position(|arg| !arg.starts_with('-')) else {
            continue;
        };
        let host = args[position];
        if position + 1 == args.len() && hosts.iter().any(|candidate| candidate == host) {
            ttys.insert(format!("/dev/{tty}"), host.to_string());
        }
    }
    ttys
}

fn status_for(binding: &RemoteBinding) -> AgentStatus {
    match binding.status.as_deref() {
        Some("working") => AgentStatus::Running,
        Some("needs_input") => AgentStatus::Waiting,
        Some("done") => AgentStatus::Done,
        _ => AgentStatus::Idle,
    }
}

pub(crate) fn build_inventory(
    bindings_by_host: &HashMap<String, Vec<RemoteBinding>>,
    proxies_by_host: &HashMap<String, Vec<LocalSshPane>>,
    now_ms: u64,
) -> RemoteInventory {
    let mut inventory = RemoteInventory::default();
    for (host, bindings) in bindings_by_host {
        // Attach to the first local pane proxying this host; with several,
        // every pane reaches the same remote tmux server.
        let Some(proxy) = proxies_by_host.get(host).and_then(|panes| panes.first()) else {
            continue;
        };
        for pane in proxies_by_host.get(host).into_iter().flatten() {
            inventory.proxy_panes.insert(pane.pane_id.clone());
        }
        for binding in bindings {
            let agent = if binding.tool == "codex" {
                "codex"
            } else {
                "claude-code"
            };
            let window_name = binding
                .window
                .as_deref()
                .and_then(|window| window.split_once(':'))
                .map(|(_, name)| name);
            let title = binding
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .or(window_name)
                .unwrap_or("Unnamed");
            let thread_id = binding
                .id
                .clone()
                .unwrap_or_else(|| format!("remote:{host}:{}", binding.pane));
            let location = match binding.window.as_deref() {
                Some(window) => format!("{host} · {window}"),
                None => host.clone(),
            };
            inventory
                .remote_targets
                .insert(thread_id.clone(), (host.clone(), binding.pane.clone()));
            if binding.active {
                inventory
                    .active_thread_by_proxy
                    .insert(proxy.pane_id.clone(), thread_id.clone());
            }
            inventory
                .agents_by_session
                .entry(proxy.session.clone())
                .or_default()
                .push(AgentEvent {
                    agent: agent.to_string(),
                    session: proxy.session.clone(),
                    status: status_for(binding),
                    ts: if binding.updated_ms > 0 { binding.updated_ms } else { now_ms },
                    thread_id: Some(thread_id),
                    thread_name: Some(title.to_string()),
                    last_user_prompt: None,
                    unseen: None,
                    pane_id: Some(proxy.pane_id.clone()),
                    liveness: Some(AgentLiveness::Alive),
                    location: Some(location),
                });
        }
    }
    inventory
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_interactive_ssh_to_configured_hosts_counts_as_a_proxy() {
        let hosts = vec!["devbox".to_string()];
        let raw = "ttys035 ssh devbox\n\
                   ttys036 ssh -t devbox\n\
                   ttys037 ssh devbox uptime\n\
                   ttys038 ssh otherhost\n\
                   ?? ssh devbox\n\
                   ttys039 /usr/bin/ssh devbox\n";
        let ttys = parse_ssh_ttys(raw, &hosts);
        assert_eq!(ttys.get("/dev/ttys035").map(String::as_str), Some("devbox"));
        assert_eq!(ttys.get("/dev/ttys036").map(String::as_str), Some("devbox"));
        assert_eq!(ttys.get("/dev/ttys039").map(String::as_str), Some("devbox"));
        assert_eq!(ttys.len(), 3);
    }

    #[test]
    fn remote_bindings_become_agents_on_the_local_proxy_pane() {
        let bindings = parse_bindings(
            "{\"tool\":\"claude\",\"pane\":\"%9\",\"window\":\"1:feature\",\"id\":\"a1\",\"status\":\"needs_input\",\"title\":\"Fill effects\",\"updated_ms\":42}\n\
             not json\n\
             {\"tool\":\"codex\",\"pane\":\"%2\",\"window\":\"2:fix-deploy\",\"status\":\"working\",\"title\":null,\"active\":true}\n",
        );
        let inventory = build_inventory(
            &HashMap::from([("devbox".to_string(), bindings)]),
            &HashMap::from([(
                "devbox".to_string(),
                vec![LocalSshPane {
                    pane_id: "%40".into(),
                    session: "remote".into(),
                }],
            )]),
            7,
        );
        let agents = &inventory.agents_by_session["remote"];
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].status, AgentStatus::Waiting);
        assert_eq!(agents[0].thread_name.as_deref(), Some("Fill effects"));
        assert_eq!(agents[0].location.as_deref(), Some("devbox · 1:feature"));
        assert_eq!(agents[0].pane_id.as_deref(), Some("%40"));
        assert_eq!((agents[0].ts, agents[1].ts), (42, 7));
        assert_eq!(agents[1].agent, "codex");
        assert_eq!(agents[1].status, AgentStatus::Running);
        // No binding: named after the remote window, keyed by host + pane.
        assert_eq!(agents[1].thread_name.as_deref(), Some("fix-deploy"));
        assert_eq!(agents[1].thread_id.as_deref(), Some("remote:devbox:%2"));
        assert_eq!(
            inventory.remote_targets.get("a1"),
            Some(&("devbox".to_string(), "%9".to_string()))
        );
        assert!(inventory.proxy_panes.contains("%40"));
        assert_eq!(
            inventory.active_thread_by_proxy.get("%40").map(String::as_str),
            Some("remote:devbox:%2")
        );
    }

    #[test]
    fn hosts_without_a_local_proxy_pane_are_not_shown() {
        let inventory = build_inventory(
            &HashMap::from([(
                "devbox".to_string(),
                parse_bindings("{\"tool\":\"claude\",\"pane\":\"%9\"}"),
            )]),
            &HashMap::new(),
            0,
        );
        assert!(inventory.agents_by_session.is_empty());
    }

    #[test]
    fn host_names_cannot_inject_ssh_options() {
        assert!(valid_host("devbox") && valid_host("my-ec2.example"));
        assert!(!valid_host("-oProxyCommand=x") && !valid_host("a b") && !valid_host(""));
    }
}
