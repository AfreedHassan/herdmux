use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use herdmux_runtime::protocol::{AgentEvent, AgentLiveness, ServerState};
use serde_json::{Value, json};

const MAX_NAME_CHARS: usize = 64;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct AgentNameError {
    pub(crate) status: &'static str,
    pub(crate) code: &'static str,
    message: &'static str,
}

impl AgentNameError {
    fn new(status: &'static str, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    pub(crate) fn unavailable() -> Self {
        Self::new(
            "503 Service Unavailable",
            "unavailable",
            "Live agent inventory is not available yet",
        )
    }

    pub(crate) fn invalid_payload() -> Self {
        Self::new(
            "400 Bad Request",
            "invalid_payload",
            "Expected JSON selectors and a name string or null",
        )
    }

    pub(crate) fn body(&self) -> Value {
        json!({"error":{"code":self.code,"message":self.message}})
    }

    fn persistence() -> Self {
        Self::new(
            "500 Internal Server Error",
            "persistence_failed",
            "Could not persist agent names; previous names were retained",
        )
    }
}

pub(crate) struct AgentNameStore {
    names: HashMap<String, String>,
    path: Option<PathBuf>,
    load_failed: bool,
}

impl AgentNameStore {
    pub(crate) fn load(path: PathBuf) -> Self {
        let loaded = match fs::read(&path) {
            Ok(raw) => parse_names(&raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(HashMap::new()),
            Err(_) => None,
        };
        let load_failed = loaded.is_none();
        Self {
            names: loaded.unwrap_or_default(),
            path: Some(path),
            load_failed,
        }
    }

    pub(crate) fn unavailable() -> Self {
        Self {
            names: HashMap::new(),
            path: None,
            load_failed: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        Self {
            names: HashMap::new(),
            path: None,
            load_failed: false,
        }
    }

    pub(crate) fn display_name(&self, agent: &AgentEvent) -> String {
        self.override_name(agent)
            .map(str::to_string)
            .unwrap_or_else(|| default_name(agent))
    }

    fn override_name(&self, agent: &AgentEvent) -> Option<&str> {
        thread_key(agent)
            .and_then(|key| self.names.get(&key))
            .or_else(|| pane_key(agent).and_then(|key| self.names.get(&key)))
            .map(String::as_str)
    }

    pub(crate) fn list(&self, agents: &[AgentEvent]) -> Value {
        json!({"agents":agents.iter().filter(|agent| is_live(agent)).map(|agent| self.descriptor(agent)).collect::<Vec<_>>()})
    }

    fn descriptor(&self, agent: &AgentEvent) -> Value {
        json!({"session":agent.session,"agent":agent.agent,"paneId":agent.pane_id,
            "threadId":agent.thread_id,"name":self.display_name(agent),
            "status":agent.status,"liveness":agent.liveness,"ts":agent.ts})
    }

    pub(crate) fn overlay_state(&self, state: &mut ServerState) {
        for session in &mut state.sessions {
            for agent in session
                .agents
                .iter_mut()
                .chain(session.agent_state.iter_mut())
            {
                if is_live(agent) {
                    agent.thread_name = Some(self.display_name(agent));
                }
            }
        }
    }

    pub(crate) fn observe(&mut self, agents: &[AgentEvent]) -> Result<(), AgentNameError> {
        let live = agents
            .iter()
            .filter(|agent| is_live(agent))
            .collect::<Vec<_>>();
        let pane_keys = live
            .iter()
            .filter_map(|agent| pane_key(agent))
            .collect::<HashSet<_>>();
        let mut proposed = self
            .names
            .iter()
            .filter(|(key, _)| !key.starts_with("pane:") || pane_keys.contains(*key))
            .map(|(key, name)| (key.clone(), name.clone()))
            .collect::<HashMap<_, _>>();
        for agent in live {
            if let (Some(thread), Some(pane)) = (thread_key(agent), pane_key(agent)) {
                if let Some(name) = proposed.remove(&pane) {
                    proposed.entry(thread).or_insert(name);
                }
            }
        }
        if proposed != self.names {
            self.replace(proposed)?;
        }
        Ok(())
    }

    pub(crate) fn rename(
        &mut self,
        agents: &[AgentEvent],
        body: &Value,
    ) -> Result<Value, AgentNameError> {
        let request = RenameRequest::parse(body)?;
        let target = request.select(agents)?;
        if let Some(key) = thread_key(target) {
            if agents.iter().filter(|agent| is_live(agent)).any(|other| {
                pane_key(other) != pane_key(target) && thread_key(other).as_deref() == Some(&key)
            }) {
                return Err(AgentNameError::new(
                    "409 Conflict",
                    "ambiguous_agent",
                    "Thread identity belongs to multiple live agents",
                ));
            }
        }
        let display = request.name.clone().unwrap_or_else(|| default_name(target));
        if agents.iter().filter(|agent| is_live(agent)).any(|other| {
            pane_key(other) != pane_key(target)
                && self.display_name(other).to_lowercase() == display.to_lowercase()
        }) {
            return Err(AgentNameError::new(
                "409 Conflict",
                "duplicate_name",
                "Another live agent already uses that name",
            ));
        }
        let mut proposed = self.names.clone();
        if let Some(key) = pane_key(target) {
            proposed.remove(&key);
        }
        if let Some(key) = thread_key(target) {
            proposed.remove(&key);
        }
        if let Some(name) = request.name {
            let key = thread_key(target)
                .or_else(|| pane_key(target))
                .ok_or_else(AgentNameError::invalid_payload)?;
            proposed.insert(key, name);
        }
        self.replace(proposed)?;
        Ok(json!({"agent":self.descriptor(target)}))
    }

    fn replace(&mut self, proposed: HashMap<String, String>) -> Result<(), AgentNameError> {
        if self.load_failed {
            return Err(AgentNameError::persistence());
        }
        if let Some(path) = &self.path {
            persist_names(path, &proposed).map_err(|error| {
                crate::debug_log(format!("agent-names persistence failed: {error}"));
                AgentNameError::persistence()
            })?;
        }
        self.names = proposed;
        Ok(())
    }
}

struct RenameRequest {
    pane: Option<String>,
    thread: Option<String>,
    session: Option<String>,
    name: Option<String>,
}

impl RenameRequest {
    fn parse(body: &Value) -> Result<Self, AgentNameError> {
        let object = body
            .as_object()
            .ok_or_else(AgentNameError::invalid_payload)?;
        let selector = |key| match object.get(key) {
            None => Ok(None),
            Some(Value::String(value))
                if !value.is_empty()
                    && value.chars().count() <= 256
                    && !value.chars().any(char::is_control) =>
            {
                Ok(Some(value.clone()))
            }
            _ => Err(AgentNameError::invalid_payload()),
        };
        let pane = selector("paneId")?;
        let thread = selector("threadId")?;
        let session = selector("session")?;
        if pane.is_none() && thread.is_none() && session.is_none() {
            return Err(AgentNameError::new(
                "400 Bad Request",
                "missing_selector",
                "Provide paneId, threadId or session",
            ));
        }
        let name = match object.get("name") {
            Some(Value::Null) => None,
            Some(Value::String(name)) => Some(validate_name(name)?.to_string()),
            _ => return Err(AgentNameError::invalid_payload()),
        };
        Ok(Self {
            pane,
            thread,
            session,
            name,
        })
    }

    fn select<'a>(&self, agents: &'a [AgentEvent]) -> Result<&'a AgentEvent, AgentNameError> {
        let mut matches = agents
            .iter()
            .filter(|agent| is_live(agent))
            .filter(|agent| {
                self.pane
                    .as_deref()
                    .is_none_or(|pane| agent.pane_id.as_deref() == Some(pane))
            })
            .filter(|agent| {
                self.thread
                    .as_deref()
                    .is_none_or(|thread| agent.thread_id.as_deref() == Some(thread))
            })
            .filter(|agent| {
                self.session
                    .as_deref()
                    .is_none_or(|session| agent.session == session)
            });
        let target = matches.next().ok_or_else(|| {
            AgentNameError::new(
                "404 Not Found",
                "agent_not_found",
                "No live agent matches all supplied selectors",
            )
        })?;
        if matches.next().is_some() {
            return Err(AgentNameError::new(
                "409 Conflict",
                "ambiguous_agent",
                "Selectors match multiple live agents; include paneId",
            ));
        }
        Ok(target)
    }
}

fn is_live(agent: &AgentEvent) -> bool {
    agent.liveness == Some(AgentLiveness::Alive) && agent.pane_id.is_some()
}

fn default_name(agent: &AgentEvent) -> String {
    let generated = format!("{} {}", agent.agent, agent.pane_id.as_deref().unwrap_or(""));
    agent
        .thread_name
        .as_deref()
        .filter(|name| !name.trim().is_empty() && *name != generated)
        .map(str::to_string)
        .unwrap_or_else(|| "Unnamed".to_string())
}

fn validate_name(name: &str) -> Result<&str, AgentNameError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(AgentNameError::new(
            "400 Bad Request",
            "invalid_name",
            "Name must contain 1–64 characters without control characters",
        ));
    }
    Ok(trimmed)
}

fn thread_key(agent: &AgentEvent) -> Option<String> {
    agent
        .thread_id
        .as_ref()
        .filter(|id| !id.is_empty() && !id.starts_with("tmux-pane:"))
        .map(|thread| format!("thread:{}", json!([agent.agent, thread])))
}

fn pane_key(agent: &AgentEvent) -> Option<String> {
    agent
        .pane_id
        .as_ref()
        .map(|pane| format!("pane:{}", json!([agent.session, agent.agent, pane])))
}

fn parse_names(raw: &[u8]) -> Option<HashMap<String, String>> {
    let value: Value = serde_json::from_slice(raw).ok()?;
    if value.get("version")?.as_u64()? != 1 {
        return None;
    }
    value
        .get("names")?
        .as_object()?
        .iter()
        .map(|(key, value)| {
            if !key.starts_with("thread:") && !key.starts_with("pane:") {
                return None;
            }
            Some((
                key.clone(),
                validate_name(value.as_str()?).ok()?.to_string(),
            ))
        })
        .collect()
}

fn persist_names(path: &std::path::Path, names: &HashMap<String, String>) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("names path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".agent-names-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let encoded_names = Value::Object(
            names
                .iter()
                .map(|(key, name)| (key.clone(), Value::String(name.clone())))
                .collect(),
        );
        file.write_all(
            &serde_json::to_vec_pretty(&json!({"version":1,"names":encoded_names}))
                .map_err(std::io::Error::other)?,
        )?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdmux_runtime::protocol::{AgentLiveness, AgentStatus};

    fn agent(pane: &str, thread: Option<&str>, name: &str) -> AgentEvent {
        AgentEvent {
            agent: "claude-code".into(),
            session: "liquid".into(),
            status: AgentStatus::Idle,
            ts: 0,
            thread_id: thread.map(str::to_string),
            thread_name: Some(name.into()),
            last_user_prompt: None,
            unseen: None,
            pane_id: Some(pane.into()),
            liveness: Some(AgentLiveness::Alive),
            location: None,
        }
    }

    #[test]
    fn names_are_shared_by_thread_and_clear_restores_raw_label() {
        let agents = vec![agent("%7", Some("thread"), "Original")];
        let mut store = AgentNameStore::memory();
        assert_eq!(
            store
                .rename(
                    &agents,
                    &serde_json::json!({"paneId":"%7","name":"API reviewer"})
                )
                .unwrap()["agent"]["name"],
            "API reviewer"
        );
        let resumed = agent("%8", Some("thread"), "Original");
        assert_eq!(store.display_name(&resumed), "API reviewer");
        assert_eq!(
            store
                .rename(
                    &[resumed],
                    &serde_json::json!({"threadId":"thread","name":null})
                )
                .unwrap()["agent"]["name"],
            "Original"
        );
    }

    #[test]
    fn contradictory_selectors_never_rename_a_different_live_agent() {
        let agents = vec![
            agent("%7", Some("one"), "One"),
            agent("%8", Some("two"), "Two"),
        ];
        let mut store = AgentNameStore::memory();
        let error = store
            .rename(
                &agents,
                &serde_json::json!({"paneId":"%7","threadId":"two","name":"Wrong"}),
            )
            .unwrap_err();
        assert_eq!(error.code, "agent_not_found");
    }

    #[test]
    fn missing_ambiguous_and_dead_selectors_are_rejected() {
        let mut dead = agent("%9", Some("dead"), "Dead");
        dead.liveness = Some(AgentLiveness::Exited);
        let agents = vec![
            agent("%7", Some("one"), "One"),
            agent("%8", Some("two"), "Two"),
            dead,
        ];
        let mut store = AgentNameStore::memory();
        for (body, expected) in [
            (serde_json::json!({"name":"Name"}), "missing_selector"),
            (
                serde_json::json!({"session":"liquid","name":"Name"}),
                "ambiguous_agent",
            ),
            (
                serde_json::json!({"paneId":"%9","name":"Name"}),
                "agent_not_found",
            ),
        ] {
            assert_eq!(store.rename(&agents, &body).unwrap_err().code, expected);
        }
    }

    #[test]
    fn names_reject_controls_length_empty_and_live_duplicates() {
        let agents = vec![
            agent("%7", Some("one"), "One"),
            agent("%8", Some("two"), "Two"),
        ];
        let mut store = AgentNameStore::memory();
        for name in [" ".to_string(), "bad\nname".to_string(), "x".repeat(65)] {
            assert_eq!(
                store
                    .rename(&agents, &serde_json::json!({"paneId":"%7","name":name}))
                    .unwrap_err()
                    .code,
                "invalid_name"
            );
        }
        assert_eq!(
            store
                .rename(&agents, &serde_json::json!({"paneId":"%7","name":" two "}))
                .unwrap_err()
                .code,
            "duplicate_name"
        );
    }

    #[test]
    fn observed_exit_removes_pane_only_names() {
        let agents = vec![agent("%7", None, "Default")];
        let mut store = AgentNameStore::memory();
        store
            .rename(
                &agents,
                &serde_json::json!({"paneId":"%7","name":"Temporary"}),
            )
            .unwrap();
        store.observe(&[]).unwrap();
        assert_eq!(store.display_name(&agents[0]), "Default");
    }

    #[test]
    fn pane_name_promotes_to_stable_thread_without_conflicting_with_thread_name() {
        let mut store = AgentNameStore::memory();
        store
            .rename(
                &[agent("%7", None, "Default")],
                &serde_json::json!({"paneId":"%7","name":"Reviewer"}),
            )
            .unwrap();
        store
            .observe(&[agent("%7", Some("one"), "Default")])
            .unwrap();
        store.observe(&[]).unwrap();
        assert_eq!(
            store.display_name(&agent("%8", Some("one"), "Default")),
            "Reviewer"
        );
    }

    #[test]
    fn persistence_failure_preserves_previous_names() {
        let blocker =
            std::env::temp_dir().join(format!("agent-name-blocker-{}", std::process::id()));
        std::fs::write(&blocker, "not a directory").unwrap();
        let mut store = AgentNameStore::load(blocker.join("names.json"));
        let original = agent("%7", Some("one"), "Original");
        let error = store
            .rename(
                &[original.clone()],
                &serde_json::json!({"paneId":"%7","name":"Renamed"}),
            )
            .unwrap_err();
        std::fs::remove_file(blocker).unwrap();
        assert_eq!(error.code, "persistence_failed");
        assert_eq!(store.display_name(&original), "Original");
    }

    #[test]
    fn stable_name_survives_store_reload_and_synthetic_identity_expires() {
        let path =
            std::env::temp_dir().join(format!("agent-name-reload-{}.json", std::process::id()));
        let agents = vec![agent("%7", Some("stable"), "Original")];
        let mut store = AgentNameStore::load(path.clone());
        store
            .rename(
                &agents,
                &serde_json::json!({"paneId":"%7","name":"api-reviewer"}),
            )
            .unwrap();
        assert_eq!(
            AgentNameStore::load(path.clone()).display_name(&agent(
                "%8",
                Some("stable"),
                "Original"
            )),
            "api-reviewer"
        );
        std::fs::remove_file(path).unwrap();
        let synthetic = agent("%9", Some("tmux-pane:claude-code:%9"), "Original");
        let mut memory = AgentNameStore::memory();
        memory
            .rename(
                &[synthetic.clone()],
                &serde_json::json!({"paneId":"%9","name":"temporary"}),
            )
            .unwrap();
        memory.observe(&[]).unwrap();
        assert_eq!(memory.display_name(&synthetic), "Original");
    }

    #[test]
    fn duplicate_stable_thread_identity_rejects_pane_selected_rename() {
        let agents = vec![
            agent("%7", Some("same"), "One"),
            agent("%8", Some("same"), "Two"),
        ];
        assert_eq!(
            AgentNameStore::memory()
                .rename(&agents, &serde_json::json!({"paneId":"%7","name":"Unique"}))
                .unwrap_err()
                .code,
            "ambiguous_agent"
        );
    }

    #[test]
    fn missing_native_name_uses_honest_placeholder() {
        assert_eq!(
            AgentNameStore::memory().display_name(&agent("%7", None, "claude-code %7")),
            "Unnamed"
        );
    }
}
