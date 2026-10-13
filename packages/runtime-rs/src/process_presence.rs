use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct ProcessTree {
    children: HashMap<u32, Vec<u32>>,
    agents: HashMap<u32, &'static str>,
}

impl ProcessTree {
    fn parse(raw: &str) -> Result<Self, String> {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut agents = HashMap::new();
        let mut count = 0;
        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let mut fields = line.split_whitespace();
            let pid = fields.next().and_then(|value| value.parse::<u32>().ok());
            let parent = fields.next().and_then(|value| value.parse::<u32>().ok());
            let command = fields.collect::<Vec<_>>().join(" ");
            let (Some(pid), Some(parent)) = (pid, parent) else {
                return Err("invalid process snapshot row".to_string());
            };
            if command.is_empty() {
                return Err("missing process executable".to_string());
            }
            children.entry(parent).or_default().push(pid);
            if let Some(agent) = agent_for_executable(&command) {
                agents.insert(pid, agent);
            }
            count += 1;
        }
        if count == 0 {
            return Err("empty process snapshot".to_string());
        }
        Ok(Self { children, agents })
    }

    pub(crate) fn agent_under(&self, pane_pid: u32) -> Option<&'static str> {
        let mut pending = vec![pane_pid];
        let mut visited = HashSet::new();
        while let Some(pid) = pending.pop() {
            if !visited.insert(pid) {
                continue;
            }
            if let Some(agent) = self.agents.get(&pid) {
                return Some(agent);
            }
            if let Some(children) = self.children.get(&pid) {
                pending.extend(children.iter().rev());
            }
        }
        None
    }
}

#[derive(Default)]
struct CachedSnapshot {
    attempted_at: Option<Instant>,
    tree: Option<Arc<ProcessTree>>,
}

pub(crate) struct ProcessCache {
    cached: Mutex<CachedSnapshot>,
    ttl: Duration,
    source: Box<dyn Fn() -> Result<String, String> + Send + Sync>,
}

impl Default for ProcessCache {
    fn default() -> Self {
        Self::with_source(Duration::from_secs(1), read_processes)
    }
}

impl ProcessCache {
    pub(crate) fn with_source(
        ttl: Duration,
        source: impl Fn() -> Result<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            cached: Mutex::new(CachedSnapshot::default()),
            ttl,
            source: Box::new(source),
        }
    }

    pub(crate) fn snapshot(&self) -> Option<Arc<ProcessTree>> {
        let mut cached = self.cached.lock().ok()?;
        if cached
            .attempted_at
            .is_none_or(|at| at.elapsed() >= self.ttl)
        {
            cached.attempted_at = Some(Instant::now());
            // Failed observations are unknown, never evidence of an agent exit.
            // Keep the last successful tree, or let the provider use fallback hints.
            if let Ok(tree) = (self.source)().and_then(|raw| ProcessTree::parse(&raw)) {
                cached.tree = Some(Arc::new(tree));
            }
        }
        cached.tree.clone()
    }
}

fn read_processes() -> Result<String, String> {
    let output = Command::new("ps")
        .args(["-ax", "-o", "pid=,ppid=,comm="])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ThreadPaneHint {
    Active(String),
    Ended,
}

pub(crate) fn thread_pane_hint(agent: &str, thread_id: &str) -> Option<ThreadPaneHint> {
    let filename = session_record_name(agent, thread_id)?;
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join(".ai/sessions").join(filename);
    let raw = std::fs::read_to_string(path).ok()?;
    parse_session_record(&raw)
}

fn session_record_name(agent: &str, thread_id: &str) -> Option<String> {
    if thread_id.is_empty()
        || !thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    let prefix = match agent {
        "claude-code" => "claude",
        "codex" => "codex",
        _ => return None,
    };
    Some(format!("{prefix}-{thread_id}.json"))
}

fn parse_session_record(raw: &str) -> Option<ThreadPaneHint> {
    let record: serde_json::Value = serde_json::from_str(raw).ok()?;
    let status = record.get("status")?.as_str()?;
    if status == "ended" {
        return Some(ThreadPaneHint::Ended);
    }
    let pane = record.get("pane")?.as_str()?;
    let number = pane.strip_prefix('%')?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) || status.is_empty() {
        return None;
    }
    Some(ThreadPaneHint::Active(pane.to_string()))
}

pub(crate) fn agent_for_executable(command: &str) -> Option<&'static str> {
    let path = Path::new(command);
    let executable = path.file_name()?.to_str()?.to_ascii_lowercase();
    if path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "versions")
        && path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|name| name == "claude")
        && executable.split('.').count() >= 3
        && executable
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Some("claude-code");
    }
    AGENT_EXECUTABLES
        .iter()
        .find_map(|(agent, names)| names.contains(&executable.as_str()).then_some(*agent))
}

const AGENT_EXECUTABLES: &[(&str, &[&str])] = &[
    ("pi", &["pi"]),
    ("amp", &["amp", "amp-local"]),
    ("claude-code", &["claude", "claude-code"]),
    ("codex", &["codex"]),
    ("gemini", &["gemini"]),
    ("cursor", &["cursor", "cursor-agent"]),
    ("antigravity", &["agy", "antigravity", "antigravity-cli"]),
    ("cline", &["cline"]),
    ("opencode", &["opencode", "open-code"]),
    ("github-copilot", &["copilot", "github-copilot", "ghcs"]),
    ("kimi", &["kimi", "kimi-code"]),
    ("kiro", &["kiro", "kiro-cli"]),
    ("droid", &["droid"]),
    ("grok", &["grok", "grok-build"]),
    ("hermes", &["hermes", "hermes-agent"]),
    ("qodercli", &["qodercli", "qoderclicn", "qoder", "qodercn"]),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn detects_native_claude_without_a_pane_title() {
        let tree =
            ProcessTree::parse("100 1 /bin/zsh\n101 100 /Users/me/.local/bin/claude\n").unwrap();
        assert_eq!(tree.agent_under(100), Some("claude-code"));
    }

    #[test]
    fn detects_claude_version_install_but_not_arbitrary_version_binary() {
        let tree = ProcessTree::parse("100 1 /bin/zsh\n101 100 /Users/me/.local/share/claude/versions/2.1.296\n200 1 /usr/bin/2.1.296\n").unwrap();
        assert_eq!(tree.agent_under(100), Some("claude-code"));
        assert_eq!(tree.agent_under(200), None);
    }

    #[test]
    fn detects_codex_native_child_under_node_launcher() {
        let tree = ProcessTree::parse("100 1 /bin/zsh\n101 100 /opt/node\n102 101 /Users/me/node_modules/@openai/codex/vendor/aarch64-apple-darwin/bin/codex\n").unwrap();
        assert_eq!(tree.agent_under(100), Some("codex"));
    }

    #[test]
    fn unrelated_agents_and_arbitrary_node_are_absent() {
        let tree = ProcessTree::parse(
            "100 1 /bin/zsh\n101 100 /opt/node\n200 1 /bin/codex\n201 1 /bin/amp\n",
        )
        .unwrap();
        assert_eq!(tree.agent_under(100), None);
    }

    #[test]
    fn exact_names_do_not_match_example_or_codex_code_mode_host() {
        let tree =
            ProcessTree::parse("100 1 /bin/example\n101 100 /bin/codex-code-mode-host\n").unwrap();
        assert_eq!(tree.agent_under(100), None);
    }

    #[test]
    fn malformed_or_empty_output_is_unknown_not_confirmed_absence() {
        assert!(ProcessTree::parse("").is_err());
        assert!(ProcessTree::parse("100 1 /bin/zsh\nbad-row").is_err());
    }

    #[test]
    fn failed_refresh_retains_last_successful_snapshot() {
        let attempts = AtomicUsize::new(0);
        let cache = ProcessCache::with_source(Duration::ZERO, move || {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok("100 1 /bin/claude".to_string())
            } else {
                Err("ps unavailable".to_string())
            }
        });
        assert_eq!(
            cache.snapshot().unwrap().agent_under(100),
            Some("claude-code")
        );
        assert_eq!(
            cache.snapshot().unwrap().agent_under(100),
            Some("claude-code")
        );
    }

    #[test]
    fn cache_reuses_one_process_observation_across_panes() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let cache = ProcessCache::with_source(Duration::from_secs(1), move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok("100 1 /bin/claude\n200 1 /bin/codex".to_string())
        });
        assert_eq!(
            cache.snapshot().unwrap().agent_under(100),
            Some("claude-code")
        );
        assert_eq!(cache.snapshot().unwrap().agent_under(200), Some("codex"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_first_observation_is_unknown() {
        let cache = ProcessCache::with_source(Duration::ZERO, || Err("ps unavailable".to_string()));
        assert!(cache.snapshot().is_none());
    }

    #[test]
    fn hook_record_names_reject_path_traversal_and_unknown_agents() {
        assert_eq!(
            session_record_name("claude-code", "abc-123"),
            Some("claude-abc-123.json".to_string())
        );
        assert_eq!(
            session_record_name("codex", "abc-123"),
            Some("codex-abc-123.json".to_string())
        );
        assert_eq!(session_record_name("claude-code", "../../secret"), None);
        assert_eq!(session_record_name("other", "abc-123"), None);
    }

    #[test]
    fn hook_record_hint_does_not_revive_ended_threads() {
        assert_eq!(
            parse_session_record(r#"{"pane":"%9","status":"running"}"#),
            Some(ThreadPaneHint::Active("%9".to_string()))
        );
        assert_eq!(
            parse_session_record(r#"{"pane":"%9","status":"ended"}"#),
            Some(ThreadPaneHint::Ended)
        );
        assert_eq!(
            parse_session_record(r#"{"pane":"not-a-pane","status":"running"}"#),
            None
        );
        assert_eq!(parse_session_record("broken json"), None);
    }
}
