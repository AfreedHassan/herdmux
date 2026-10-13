use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use herdmux_runtime::agent_watchers::{
    AgentWatcherSnapshot, amp_snapshot_from_thread_json, claude_code_snapshot_from_jsonl,
    codex_snapshot_from_jsonl, codex_thread_id_from_path, decode_claude_project_dir,
    droid_snapshot_from_jsonl, opencode_snapshot_from_row, parse_codex_session_index,
    pi_snapshot_from_jsonl,
};
use herdmux_runtime::config::{
    HerdmuxConfig, load_config_from_home, save_config_to_home,
};
use herdmux_runtime::git_info::{GitInfo, parse_git_info_output};
use herdmux_runtime::metadata_store::SessionMetadataStore;
use herdmux_runtime::mux::{ActiveWindow, MuxProvider, SidebarPosition};
use herdmux_runtime::pi_runtime_registry::{PiRuntimeRegistry, parse_pi_runtime_info};
use herdmux_runtime::port_discovery::{PortDiscoveryInput, discover_session_ports};
use herdmux_runtime::project_dir_session::{
    build_dir_session_map, resolve_session_for_project_dir,
};
use herdmux_runtime::protocol::{
    AgentEvent, AgentLiveness, AgentPanelScope, AgentStatus, MetadataTone, ServerMessage,
    ServerState, SessionFilterMode,
};
use herdmux_runtime::server_state::{ReadOnlyStateInput, build_read_only_state};
use herdmux_runtime::session_order::SessionOrder;
use herdmux_runtime::sidebar_coordinator::SidebarCoordinator;
use herdmux_runtime::sidebar_width_sync::clamp_sidebar_width;
use herdmux_runtime::tmux_provider::{StdCommandRunner, TmuxProvider};
use herdmux_runtime::tracker::{AgentTracker, PanePresenceInput};
use herdmux_sidebar_core::app::App as SidebarApp;
use herdmux_sidebar_core::generated::protocol::ServerMessage as SidebarServerMessage;
use serde_json::Value;
use sha1_smol::Sha1;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{Duration, MissedTickBehavior};
use tokio_websockets::{Message, ServerBuilder};

mod agent_names;
mod remote_agents;
pub use agent_names::AgentNameError;
use agent_names::AgentNameStore;

pub const SERVER_VERSION: &str = "0.2.0-alpha.12";
pub const PROTOCOL_VERSION: u16 = 1;
pub const HELLO_JSON: &str = r#"{"type":"hello","protocol":1,"serverVersion":"0.2.0-alpha.12"}"#;
pub const QUIT_JSON: &str = r#"{"type":"quit"}"#;

const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const SIDEBAR_SCRIPTS_DIR: &str = "apps/tui/scripts";
const GIT_CACHE_TTL_MS: u64 = 5_000;
const PORT_POLL_INTERVAL_MS: u64 = 10_000;
const RENDERED_SIDEBAR_FRAME_MS: u64 = 16;
const AGENT_WATCHER_POLL_MS: u64 = 2_000;
const TMUX_STATE_POLL_MS: u64 = 2_000;
const SIDEBAR_WARMUP_MS: u64 = 1_200;
const SERVER_SHUTDOWN_DRAIN_MS: u64 = 120;
const AGENT_WATCHER_RECENT_MS: u64 = 5 * 60 * 1000;
const AGENT_FOCUS_COOLDOWN_MS: u64 = 500;
const OPENCODE_SQL_TIMEOUT_MS: u64 = 500;
const OPENCODE_SQL_SEP: char = '\u{1f}';
const DEFAULT_DETAIL_PANEL_HEIGHT: u16 = 10;
const MIN_DETAIL_PANEL_HEIGHT: u16 = 4;
const MAX_DETAIL_PANEL_HEIGHT: u16 = 60;

#[derive(Debug, Default)]
struct ShutdownAnnouncement {
    announced: AtomicBool,
}

impl ShutdownAnnouncement {
    fn announce_once(
        &self,
        state_source: &Option<Arc<dyn StateSource>>,
        state_updates: &broadcast::Sender<String>,
    ) {
        if self.announced.swap(true, Ordering::SeqCst) {
            return;
        }
        announce_shutdown(state_source, state_updates);
    }
}

/// Append a single debug line. Temporarily defaults to `/tmp/herdmux-debug.log`
/// so live focus/agent-state issues can be diagnosed without extra env setup;
/// `HERDMUX_DEBUG_LOG` still overrides the path when set.
fn debug_log(line: impl AsRef<str>) {
    use std::io::Write;
    let path = std::env::var("HERDMUX_DEBUG_LOG")
        .ok()
        .unwrap_or_else(|| "/tmp/herdmux-debug.log".to_string());
    if path.is_empty() {
        return;
    }
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(
            file,
            "[{now}] [server pid={}] {}",
            std::process::id(),
            line.as_ref()
        );
    }
}

pub trait StateSource: Send + Sync + 'static {
    fn snapshot_json(&self) -> String;
    fn cached_snapshot_json(&self) -> Option<String> {
        None
    }

    fn sender_agent_focus_json(&self, _context: &ClientConnectionContext) -> Option<String> {
        None
    }

    fn setup_mux_hooks(&self, _server_host: &str, _server_port: u16) {}

    fn cleanup_mux_hooks(&self) {}

    fn start_background_tasks(
        self: Arc<Self>,
        _state_updates: broadcast::Sender<String>,
        _shutdown: broadcast::Sender<()>,
    ) -> Vec<JoinHandle<()>> {
        Vec::new()
    }

    fn handle_client_command(&self, _command: &Value) -> Option<String> {
        None
    }

    fn handle_client_command_with_context(
        &self,
        command: &Value,
        _context: Option<&ClientConnectionContext>,
    ) -> Option<String> {
        self.handle_client_command(command)
    }

    fn handle_sender_command(&self, _command: &Value) -> Option<String> {
        None
    }

    fn handle_sender_command_with_context(
        &self,
        command: &Value,
        _context: &mut ClientConnectionContext,
    ) -> Option<String> {
        self.handle_sender_command(command)
    }

    fn handle_http_json(&self, _path: &str, _body: &Value) -> Option<String> {
        None
    }

    fn handle_http_text(&self, _path: &str, _body: &str) -> Option<String> {
        None
    }

    fn handle_http_hook(&self, _path: &str, _body: &str) -> Option<String> {
        None
    }

    fn handle_switch_index(&self, _index: u32, _body: &str) -> Option<String> {
        None
    }

    fn handle_agent_event_json(&self, _body: &Value) -> Result<String, AgentEventError> {
        Err(AgentEventError::CouldNotResolveSession)
    }

    fn agent_list_json(&self) -> Result<Value, AgentNameError> {
        Err(AgentNameError::unavailable())
    }

    fn rename_agent_json(&self, _body: &Value) -> Result<(Value, Option<String>), AgentNameError> {
        Err(AgentNameError::unavailable())
    }

    fn handle_pi_runtime_upsert(&self, _body: &Value) -> Result<(), PiRuntimeError> {
        Err(PiRuntimeError::InvalidPayload)
    }

    fn handle_pi_runtime_delete(&self, _body: &Value) -> Result<(), PiRuntimeError> {
        Err(PiRuntimeError::MissingPid)
    }

    fn begin_shutdown(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClientConnectionContext {
    client_tty: Option<String>,
    pane_id: Option<String>,
    session_name: Option<String>,
    window_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEventError {
    MissingAgent,
    InvalidStatus,
    CouldNotResolveSession,
}

impl AgentEventError {
    fn status_and_body(self) -> (&'static str, &'static str) {
        match self {
            Self::MissingAgent => ("400 Bad Request", "missing agent"),
            Self::InvalidStatus => ("400 Bad Request", "invalid status"),
            // Agent events are intentionally broadcast to every herdmux
            // server in every tmux namespace. A server that cannot map the
            // event's projectDir/tmuxSession to one of its sessions should
            // no-op with a non-error status so the plugin can publish once and
            // let each server decide folder ownership locally. Use 202 (not
            // 204) so the plugin can distinguish "ignored by this server" from
            // "applied by an owning server" when deciding whether to retry
            // during owner-server restarts.
            Self::CouldNotResolveSession => ("202 Accepted", ""),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiRuntimeError {
    InvalidPayload,
    MissingPid,
}

impl PiRuntimeError {
    fn body(self) -> &'static str {
        match self {
            Self::InvalidPayload => "invalid pi runtime payload",
            Self::MissingPid => "missing pid",
        }
    }
}

impl<F> StateSource for F
where
    F: Fn() -> String + Send + Sync + 'static,
{
    fn snapshot_json(&self) -> String {
        self()
    }
}

pub trait PortCommandRunner: Send + Sync + 'static {
    fn process_rows(&self) -> Vec<(u32, u32)>;
    fn lsof_fields(&self) -> String;
}

pub trait GitCommandRunner: Send + Sync + 'static {
    fn git_info_output(&self, dir: &str) -> String;
}

#[derive(Debug, Default)]
struct SystemPortCommandRunner;

#[derive(Debug, Default)]
struct SystemGitCommandRunner;

impl PortCommandRunner for SystemPortCommandRunner {
    fn process_rows(&self) -> Vec<(u32, u32)> {
        let Ok(output) = process::Command::new("ps")
            .args(["-eo", "pid=,ppid="])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_process_row)
            .collect()
    }

    fn lsof_fields(&self) -> String {
        let Ok(output) = process::Command::new("/usr/sbin/lsof")
            .args(["-iTCP", "-sTCP:LISTEN", "-nP", "-F", "pn"])
            .output()
        else {
            return String::new();
        };
        if !output.status.success() {
            return String::new();
        }
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

impl GitCommandRunner for SystemGitCommandRunner {
    fn git_info_output(&self, dir: &str) -> String {
        if dir.is_empty() {
            return String::new();
        }

        let Ok(rev_parse) = process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD", "--git-dir"])
            .output()
        else {
            return String::new();
        };
        if !rev_parse.status.success() {
            return String::new();
        }

        let Ok(status) = process::Command::new("git")
            .current_dir(dir)
            .args(["status", "--porcelain"])
            .output()
        else {
            return String::new();
        };

        let Ok(numstat) = process::Command::new("git")
            .current_dir(dir)
            .args(["diff", "--numstat", "HEAD", "--"])
            .output()
        else {
            return String::new();
        };

        format!(
            "{}\n---\n{}\n---NUMSTAT---\n{}",
            String::from_utf8_lossy(&rev_parse.stdout).trim(),
            String::from_utf8_lossy(&status.stdout).trim(),
            String::from_utf8_lossy(&numstat.stdout).trim()
        )
    }
}

#[derive(Debug, Clone)]
struct CachedGitInfo {
    info: GitInfo,
    ts: u64,
}

#[derive(Debug, Clone)]
struct CachedPortSnapshot {
    session_names: Vec<String>,
    ports_by_session: HashMap<String, Vec<u16>>,
    ts: u64,
}

pub struct ReadOnlyMuxStateSource {
    providers: Vec<Arc<dyn MuxProvider>>,
    remote_hosts: Vec<String>,
    remote_inventory: Mutex<remote_agents::RemoteInventory>,
    port_command_runner: Arc<dyn PortCommandRunner>,
    port_snapshot_cache: Mutex<Option<CachedPortSnapshot>>,
    git_command_runner: Arc<dyn GitCommandRunner>,
    git_info_cache: Mutex<HashMap<String, CachedGitInfo>>,
    // The sidebar coordinator owns the single source of truth for the current
    // width (`SidebarCoordinator::state().width`), so there is no separate
    // mirror field to drift out of sync.
    sidebar_coordinator: Mutex<SidebarCoordinator>,
    detail_panel_height: Mutex<u16>,
    agent_panel_scope: Mutex<AgentPanelScope>,
    focused_session: Mutex<Option<String>>,
    focused_pane_by_session: Mutex<HashMap<String, String>>,
    last_explicit_agent_focus: Mutex<Option<(String, String, Instant)>>,
    focused_client_tty: Mutex<Option<String>>,
    agent_focus_by_client: Mutex<HashMap<Option<String>, (String, String)>>,
    last_focus_hook_by_client: Mutex<HashMap<Option<String>, (String, String)>>,
    theme: Mutex<Option<String>>,
    session_filter: Mutex<Option<SessionFilterMode>>,
    collapsed_worktree_groups: Mutex<HashSet<String>>,
    session_order: Mutex<SessionOrder>,
    metadata_store: Mutex<SessionMetadataStore>,
    agent_tracker: Mutex<AgentTracker>,
    confirmed_agent_panes: Mutex<HashMap<String, Vec<PanePresenceInput>>>,
    confirmed_inventory_ready: AtomicBool,
    agent_names: Mutex<AgentNameStore>,
    cached_snapshot: Mutex<Option<ServerState>>,
    pi_runtime_registry: Mutex<PiRuntimeRegistry>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

fn default_agent_name_store() -> AgentNameStore {
    #[cfg(test)]
    {
        AgentNameStore::memory()
    }
    #[cfg(not(test))]
    {
        AgentNameStore::unavailable()
    }
}

fn agents_with_live_panes(
    session: &str,
    tracked: Vec<AgentEvent>,
    panes: &[PanePresenceInput],
) -> Vec<AgentEvent> {
    let mut seen = HashSet::new();
    let mut agents = panes
        .iter()
        .filter(|pane| seen.insert((pane.agent.as_str(), pane.pane_id.as_str())))
        .map(|pane| {
            let mut event = tracked
                .iter()
                .filter(|event| {
                    event.agent == pane.agent
                        && event.pane_id.as_deref() == Some(&pane.pane_id)
                        && event.liveness == Some(AgentLiveness::Alive)
                        && pane
                            .thread_id
                            .as_deref()
                            .is_none_or(|thread| event.thread_id.as_deref() == Some(thread))
                })
                .max_by_key(|event| event.ts)
                .cloned()
                .unwrap_or_else(|| AgentEvent {
                    agent: pane.agent.clone(),
                    session: session.to_string(),
                    status: AgentStatus::Idle,
                    ts: 0,
                    thread_id: pane.thread_id.clone(),
                    thread_name: pane.thread_name.clone(),
                    last_user_prompt: None,
                    unseen: None,
                    pane_id: Some(pane.pane_id.clone()),
                    liveness: Some(AgentLiveness::Alive),
                    location: None,
                });
            if let Some(title) = pane
                .thread_name
                .as_ref()
                .filter(|title| !title.trim().is_empty())
            {
                event.thread_name = Some(title.clone());
            }
            // Hook-reported questions/permission prompts beat transcript
            // heuristics: Claude writes the question only after it is answered.
            if pane.needs_input {
                event.status = AgentStatus::Waiting;
            } else if pane.working
                && matches!(
                    event.status,
                    AgentStatus::Idle | AgentStatus::Done | AgentStatus::Stale
                )
            {
                // The turn has not ended (Stop hook not fired), e.g. the main
                // transcript is quiet while background agents run.
                event.status = AgentStatus::Running;
            }
            event
        })
        .collect::<Vec<_>>();
    agents.extend(tracked.into_iter().filter(|event| event.liveness.is_none()));
    agents
}

pub fn default_state_source_from_env(
    env: impl Fn(&str) -> Option<String>,
) -> Option<ReadOnlyMuxStateSource> {
    if env("TMUX").is_some() {
        let provider = Arc::new(TmuxProvider::new(Arc::new(StdCommandRunner::default())));
        let mut source = ReadOnlyMuxStateSource::new(vec![provider]);
        let home = env("HOME").map(PathBuf::from);
        let config = home.as_deref().map(load_config_from_home);
        source.remote_hosts = home
            .as_deref()
            .map(remote_agents::load_hosts)
            .unwrap_or_default();
        let names_file = herdmux_runtime::shared::resolve_server_key(&env)
            .and_then(|key| key.parse::<u16>().ok())
            .map(|key| format!("agent-names.{key}.json"))
            .unwrap_or_else(|| "agent-names.default.json".to_string());
        source.agent_names = Mutex::new(
            home.map(|home| {
                AgentNameStore::load(home.join(".config/herdmux").join(names_file))
            })
            .unwrap_or_else(AgentNameStore::unavailable),
        );
        if let Some(width) = config.as_ref().and_then(|config| config.sidebar_width) {
            source = source.with_sidebar_width(clamp_sidebar_width(width) as u32);
        }
        if let Some(height) = config
            .as_ref()
            .and_then(|config| config.detail_panel_height)
        {
            source = source.with_detail_panel_height(height);
        }
        source.session_filter =
            Mutex::new(config.as_ref().and_then(|config| config.session_filter));
        if let Some(scope) = config.and_then(|config| config.agent_panel_scope) {
            source.agent_panel_scope = Mutex::new(scope);
        }
        return Some(source);
    }

    None
}

impl ReadOnlyMuxStateSource {
    pub fn new(providers: Vec<Arc<dyn MuxProvider>>) -> Self {
        Self {
            providers,
            port_command_runner: Arc::new(SystemPortCommandRunner),
            port_snapshot_cache: Mutex::new(None),
            git_command_runner: Arc::new(SystemGitCommandRunner),
            git_info_cache: Mutex::new(HashMap::new()),
            sidebar_coordinator: Mutex::new(SidebarCoordinator::new(26)),
            detail_panel_height: Mutex::new(DEFAULT_DETAIL_PANEL_HEIGHT),
            agent_panel_scope: Mutex::new(AgentPanelScope::Current),
            focused_session: Mutex::new(None),
            focused_pane_by_session: Mutex::new(HashMap::new()),
            last_explicit_agent_focus: Mutex::new(None),
            focused_client_tty: Mutex::new(None),
            agent_focus_by_client: Mutex::new(HashMap::new()),
            last_focus_hook_by_client: Mutex::new(HashMap::new()),
            theme: Mutex::new(None),
            session_filter: Mutex::new(None),
            collapsed_worktree_groups: Mutex::new(HashSet::new()),
            session_order: Mutex::new(SessionOrder::new(None)),
            metadata_store: Mutex::new(SessionMetadataStore::new()),
            agent_tracker: Mutex::new(AgentTracker::new()),
            confirmed_agent_panes: Mutex::new(HashMap::new()),
            confirmed_inventory_ready: AtomicBool::new(false),
            remote_hosts: Vec::new(),
            remote_inventory: Mutex::new(remote_agents::RemoteInventory::default()),
            agent_names: Mutex::new(default_agent_name_store()),
            cached_snapshot: Mutex::new(None),
            pi_runtime_registry: Mutex::new(PiRuntimeRegistry::with_default_ttl()),
            now_ms: Arc::new(current_time_ms),
        }
    }

    pub fn with_sidebar_width(mut self, sidebar_width: u32) -> Self {
        self.sidebar_coordinator = Mutex::new(SidebarCoordinator::new(sidebar_width));
        self
    }

    pub fn with_detail_panel_height(mut self, height: u16) -> Self {
        self.detail_panel_height = Mutex::new(clamp_detail_panel_height(height));
        self
    }

    /// Current sidebar width from the coordinator (single source of truth),
    /// clamped to `u16` for the tmux resize APIs.
    fn current_sidebar_width_u16(&self) -> u16 {
        self.sidebar_coordinator
            .lock()
            .unwrap()
            .state()
            .width
            .min(u16::MAX as u32) as u16
    }

    fn is_sidebar_visible(&self) -> bool {
        self.sidebar_coordinator.lock().unwrap().state().visible
    }

    fn persist_sidebar_width(&self, width: u16) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            debug_log("set-sidebar-width: skipped config save because HOME is unset");
            return;
        };
        if let Err(err) = save_config_to_home(
            &home,
            HerdmuxConfig {
                sidebar_width: Some(width),
                ..HerdmuxConfig::default()
            },
        ) {
            debug_log(format!(
                "set-sidebar-width: failed to save sidebarWidth={width}: {err}"
            ));
        }
    }

    fn persist_detail_panel_height(&self, height: u16) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            debug_log("set-detail-panel-height: skipped config save because HOME is unset");
            return;
        };
        if let Err(err) = save_config_to_home(
            &home,
            HerdmuxConfig {
                detail_panel_height: Some(height),
                ..HerdmuxConfig::default()
            },
        ) {
            debug_log(format!(
                "set-detail-panel-height: failed to save detailPanelHeight={height}: {err}"
            ));
        }
    }

    pub fn with_now_ms(mut self, now_ms: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now_ms = Arc::new(now_ms);
        self
    }

    pub fn with_port_command_runner(mut self, runner: Arc<dyn PortCommandRunner>) -> Self {
        self.port_command_runner = runner;
        self
    }

    pub fn with_git_command_runner(mut self, runner: Arc<dyn GitCommandRunner>) -> Self {
        self.git_command_runner = runner;
        self
    }

    fn sync_agent_pane_presence(&self) -> bool {
        let mut presence_by_session = Vec::new();
        let mut focused_agent_panes = HashMap::<String, String>::new();
        let mut client_agent_focus = Vec::new();
        let focused_client_tty = self.focused_client_tty.lock().unwrap().clone();
        for provider in &self.providers {
            let client_focus = provider.get_client_focus(focused_client_tty.as_deref());
            for session in provider.list_sessions() {
                let pane_agents = provider
                    .list_agent_panes(&session.name)
                    .into_iter()
                    .map(|pane| {
                        if client_focus.as_ref().is_some_and(|focus| {
                            focus.session_name == session.name && focus.pane_id == pane.pane_id
                        }) {
                            focused_agent_panes.insert(session.name.clone(), pane.pane_id.clone());
                            if let Some(focus) = &client_focus {
                                client_agent_focus.push((
                                    focus.client_tty.clone(),
                                    (session.name.clone(), pane.pane_id.clone()),
                                ));
                            }
                        }
                        PanePresenceInput {
                            agent: pane.agent,
                            pane_id: pane.pane_id,
                            active: pane.active,
                            thread_id: pane.thread_id,
                            thread_name: pane.thread_name,
                            needs_input: pane.needs_input,
                            working: pane.working,
                        }
                    })
                    .collect::<Vec<_>>();
                if !pane_agents.is_empty() {
                    debug_log(format!(
                        "agent-pane-presence session={} panes={:?}",
                        session.name, pane_agents,
                    ));
                }
                presence_by_session.push((session.name, pane_agents));
            }
        }

        *self.confirmed_agent_panes.lock().unwrap() = presence_by_session.iter().cloned().collect();
        self.confirmed_inventory_ready
            .store(true, Ordering::Release);
        for (tty, focus) in client_agent_focus {
            self.agent_focus_by_client
                .lock()
                .unwrap()
                .insert(tty, focus);
        }
        let mut changed = false;
        let mut tracker = self.agent_tracker.lock().unwrap();
        for (session, pane_agents) in presence_by_session {
            changed = tracker.apply_pane_presence(&session, pane_agents) || changed;
        }
        for (session, pane_id) in focused_agent_panes {
            let previous = self
                .focused_pane_by_session
                .lock()
                .unwrap()
                .insert(session.clone(), pane_id.clone());
            let seen_changed = tracker.mark_pane_seen(&session, &pane_id);
            debug_log(format!(
                "current-agent-pane-seen session={session} pane={pane_id} previous={previous:?} changed={seen_changed}",
            ));
            changed = seen_changed || changed;
        }
        changed
    }

    fn mark_focused_agent_panes_seen(&self) -> bool {
        let focused = self.focused_pane_by_session.lock().unwrap().clone();
        if focused.is_empty() {
            return false;
        }
        let mut tracker = self.agent_tracker.lock().unwrap();
        let mut changed = false;
        for (session, pane_id) in focused {
            let pane_changed = tracker.mark_pane_seen(&session, &pane_id);
            debug_log(format!(
                "focused-pane-seen-check session={session} pane={pane_id} changed={pane_changed}",
            ));
            changed = pane_changed || changed;
        }
        changed
    }

    fn remember_focused_pane(&self, context: &HttpContext) -> bool {
        if context.pane_active == Some(false) {
            debug_log(format!(
                "focus-pane ignored inactive session={} pane={:?}",
                context.session, context.pane_id,
            ));
            return false;
        }
        let Some(pane_id) = context
            .pane_id
            .as_deref()
            .filter(|pane_id| !pane_id.is_empty())
        else {
            return false;
        };
        if context.client_tty.is_some() {
            *self.focused_client_tty.lock().unwrap() = context.client_tty.clone();
        }
        self.focused_pane_by_session
            .lock()
            .unwrap()
            .insert(context.session.clone(), pane_id.to_string());
        let changed = self
            .agent_tracker
            .lock()
            .unwrap()
            .mark_pane_seen(&context.session, pane_id);
        debug_log(format!(
            "focus-pane session={} pane={} changed={changed}",
            context.session, pane_id,
        ));
        changed
    }

    /// Local agent panes plus ssh panes proxying remote agents.
    fn is_agent_focus_pane(&self, session: &str, pane: &str) -> bool {
        self.is_confirmed_agent_pane(session, pane)
            || self
                .remote_inventory
                .lock()
                .unwrap()
                .proxy_panes
                .contains(pane)
    }

    fn remote_active_thread(&self, pane: &str) -> Option<String> {
        self.remote_inventory
            .lock()
            .unwrap()
            .active_thread_by_proxy
            .get(pane)
            .cloned()
    }

    fn is_confirmed_agent_pane(&self, session: &str, pane: &str) -> bool {
        self.confirmed_agent_panes
            .lock()
            .unwrap()
            .get(session)
            .is_some_and(|panes| panes.iter().any(|live| live.pane_id == pane))
    }

    fn remember_agent_focus(&self, tty: Option<String>, session: &str, pane: &str) {
        self.agent_focus_by_client
            .lock()
            .unwrap()
            .insert(tty.clone(), (session.to_string(), pane.to_string()));
        self.last_focus_hook_by_client
            .lock()
            .unwrap()
            .insert(tty, (session.to_string(), pane.to_string()));
    }
}

impl StateSource for ReadOnlyMuxStateSource {
    fn setup_mux_hooks(&self, server_host: &str, server_port: u16) {
        let width = self.current_sidebar_width_u16();
        for provider in &self.providers {
            provider.set_sidebar_width_hint(width);
            provider.setup_hooks(server_host, server_port);
        }
    }

    fn cleanup_mux_hooks(&self) {
        for provider in &self.providers {
            provider.cleanup_hooks();
        }
    }

    fn start_background_tasks(
        self: Arc<Self>,
        state_updates: broadcast::Sender<String>,
        shutdown: broadcast::Sender<()>,
    ) -> Vec<JoinHandle<()>> {
        vec![
            tokio::spawn(run_agent_watcher_loop(
                self.clone(),
                state_updates.clone(),
                shutdown.clone(),
            )),
            tokio::spawn(run_sidebar_lifecycle_loop(
                self.clone(),
                state_updates.clone(),
                shutdown.clone(),
            )),
            tokio::spawn(run_remote_agent_loop(
                self.clone(),
                state_updates.clone(),
                shutdown.clone(),
            )),
            tokio::spawn(run_tmux_state_poll_loop(self, state_updates, shutdown)),
        ]
    }

    fn snapshot_json(&self) -> String {
        self.sync_agent_pane_presence();
        self.mark_focused_agent_panes_seen();

        let providers = self
            .providers
            .iter()
            .map(|provider| provider.as_ref())
            .collect::<Vec<_>>();
        let visible_session_names = self.visible_session_names();
        let metadata_by_session = visible_session_names.as_ref().map(|names| {
            names
                .iter()
                .filter_map(|name| {
                    self.metadata_store
                        .lock()
                        .unwrap()
                        .get(name)
                        .map(|metadata| (name.clone(), metadata))
                })
                .collect()
        });
        let git_by_session = self.git_info_by_session(visible_session_names.as_deref());
        let (agent_state_by_session, agents_by_session, event_timestamps_by_session) =
            visible_session_names
                .as_ref()
                .map(|names| {
                    let tracker = self.agent_tracker.lock().unwrap();
                    let confirmed_panes = self.confirmed_agent_panes.lock().unwrap();
                    let mut states = HashMap::new();
                    let mut agents = HashMap::new();
                    let mut timestamps = HashMap::new();
                    let remote = self.remote_inventory.lock().unwrap();
                    for name in names {
                        let mut session_agents = agents_with_live_panes(
                            name,
                            tracker.get_agents(name),
                            confirmed_panes
                                .get(name)
                                .map(Vec::as_slice)
                                .unwrap_or_default(),
                        );
                        if let Some(remote_agents) = remote.agents_by_session.get(name) {
                            session_agents.extend(remote_agents.iter().cloned());
                        }
                        if let Some(state) = session_agents
                            .iter()
                            .filter(|agent| agent.liveness == Some(AgentLiveness::Alive))
                            .max_by_key(|agent| agent.ts)
                            .cloned()
                            .or_else(|| tracker.get_state(name))
                        {
                            states.insert(name.clone(), state);
                        }
                        if !session_agents.is_empty() {
                            agents.insert(name.clone(), session_agents);
                        }
                        let session_timestamps = tracker.get_event_timestamps(name);
                        if !session_timestamps.is_empty() {
                            timestamps.insert(name.clone(), session_timestamps);
                        }
                    }
                    (Some(states), Some(agents), Some(timestamps))
                })
                .unwrap_or((None, None, None));
        let ports_by_session = self.discover_live_ports(visible_session_names.as_deref());
        let sidebar_state = self.sidebar_coordinator.lock().unwrap().state();
        debug_log(format!(
            "snapshot_json mode={} init={} width={}",
            sidebar_state.mode, sidebar_state.initializing, sidebar_state.width,
        ));
        let mut state = build_read_only_state(ReadOnlyStateInput {
            providers,
            visible_session_names,
            metadata_by_session,
            git_by_session,
            agent_state_by_session,
            agents_by_session,
            event_timestamps_by_session,
            unseen_sessions: Some(self.agent_tracker.lock().unwrap().get_unseen()),
            ports_by_session,
            portless_state: None,
            focused_session: self.focused_session.lock().unwrap().clone(),
            current_session_override: None,
            theme: self.theme.lock().unwrap().clone(),
            session_filter: *self.session_filter.lock().unwrap(),
            agent_panel_scope: *self.agent_panel_scope.lock().unwrap(),
            collapsed_worktree_groups: self
                .collapsed_worktree_groups
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect(),
            sidebar_width: sidebar_state.width,
            detail_panel_height: u32::from(*self.detail_panel_height.lock().unwrap()),
            initializing: sidebar_state.initializing,
            init_label: (!sidebar_state.init_label.is_empty()).then_some(sidebar_state.init_label),
            now_ms: (self.now_ms)(),
        });

        *self.cached_snapshot.lock().unwrap() = Some(state.clone());
        let live_agents = self.cached_live_agents();
        let mut names = self.agent_names.lock().unwrap();
        if let Ok(live_agents) = live_agents {
            if let Err(error) = names.observe(&live_agents) {
                debug_log(format!("agent-name observation failed: {}", error.code));
            }
        }
        names.overlay_state(&mut state);
        serde_json::to_string(&ServerMessage::State(state)).expect("state must serialize")
    }

    fn cached_snapshot_json(&self) -> Option<String> {
        let mut state = self.cached_snapshot.lock().unwrap().clone()?;
        self.agent_names.lock().unwrap().overlay_state(&mut state);
        Some(serde_json::to_string(&ServerMessage::State(state)).expect("state must serialize"))
    }

    fn sender_agent_focus_json(&self, context: &ClientConnectionContext) -> Option<String> {
        let (session, pane) = self
            .agent_focus_by_client
            .lock()
            .unwrap()
            .get(&context.client_tty)
            .cloned()?;
        if context.session_name.as_deref() != Some(session.as_str())
            || !self.is_agent_focus_pane(&session, &pane)
        {
            return None;
        }
        let thread = self.remote_active_thread(&pane);
        Some(activate_agent_pane_json(
            session,
            &pane,
            context.client_tty.as_deref(),
            thread.as_deref(),
        ))
    }

    fn begin_shutdown(&self) -> Option<String> {
        {
            let mut coordinator = self.sidebar_coordinator.lock().unwrap();
            coordinator.begin_closing();
        }
        Some(self.snapshot_json())
    }

    fn handle_client_command(&self, command: &Value) -> Option<String> {
        self.handle_client_command_with_context(command, None)
    }

    fn handle_client_command_with_context(
        &self,
        command: &Value,
        context: Option<&ClientConnectionContext>,
    ) -> Option<String> {
        let provider = self.providers.first()?;
        match command.get("type").and_then(Value::as_str)? {
            "new-session" => {
                provider.create_session(None, None);
                Some(self.snapshot_json())
            }
            "switch-session" => {
                let name = command.get("name")?.as_str()?;
                let client_tty = command
                    .get("clientTty")
                    .and_then(Value::as_str)
                    .or_else(|| context.and_then(|context| context.client_tty.as_deref()));
                provider.switch_session(name, client_tty);
                None
            }
            "switch-index" => {
                let index = command.get("index")?.as_u64()?.min(u32::MAX as u64) as u32;
                self.switch_visible_index(index, None)
            }
            "kill-session" => {
                let name = command.get("name")?.as_str()?;
                if provider.get_current_session().as_deref() == Some(name)
                    && let Some(next) = self
                        .session_before(name)
                        .or_else(|| self.session_after(name))
                {
                    provider.switch_session(&next, None);
                    *self.focused_session.lock().unwrap() = Some(next);
                }
                provider.kill_session(name);
                Some(self.snapshot_json())
            }
            "hide-session" => {
                let name = command.get("name")?.as_str()?;
                self.session_order.lock().unwrap().hide(name);
                Some(self.snapshot_json())
            }
            "show-all-sessions" => {
                self.session_order.lock().unwrap().show_all();
                Some(self.snapshot_json())
            }
            "reorder-session" => {
                let name = command.get("name")?.as_str()?;
                let delta = command.get("delta")?.as_i64()? as i8;
                if let Some(names) = self.sidebar_reordered_session_names(name, delta) {
                    self.session_order.lock().unwrap().set_visible_order(names);
                }
                Some(self.snapshot_json())
            }
            "reorder-worktree-group" => {
                let key = command.get("key")?.as_str()?;
                let delta = command.get("delta")?.as_i64()? as i8;
                if let Some(names) = self.sidebar_reordered_worktree_group_names(key, delta) {
                    self.session_order.lock().unwrap().set_visible_order(names);
                }
                Some(self.snapshot_json())
            }
            "set-theme" => {
                let theme = command.get("theme")?.as_str()?.to_string();
                *self.theme.lock().unwrap() = Some(theme);
                Some(self.snapshot_json())
            }
            "set-sidebar-width" => {
                let width = command.get("width")?.as_u64()?.min(u16::MAX as u64) as u16;
                let width = clamp_sidebar_width(width);
                self.persist_sidebar_width(width);
                self.sidebar_coordinator
                    .lock()
                    .unwrap()
                    .set_width(u32::from(width));
                for provider in &self.providers {
                    provider.set_sidebar_width_hint(width);
                }
                self.enforce_sidebar_width(width);
                Some(self.snapshot_json())
            }
            "set-detail-panel-height" => {
                let height = command.get("height")?.as_u64()?.min(u16::MAX as u64) as u16;
                let height = clamp_detail_panel_height(height);
                *self.detail_panel_height.lock().unwrap() = height;
                self.persist_detail_panel_height(height);
                Some(self.snapshot_json())
            }
            "set-agent-panel-scope" => {
                let scope = parse_agent_panel_scope(command.get("scope")?.as_str()?)?;
                *self.agent_panel_scope.lock().unwrap() = scope;
                Some(self.snapshot_json())
            }
            "repair-width" => {
                if self.is_sidebar_visible() {
                    let width = self.current_sidebar_width_u16();
                    if !self.repair_context_sidebar_width(context, width) {
                        self.enforce_sidebar_width(width);
                    }
                }
                None
            }
            "set-filter" => {
                let filter = match command.get("filter")?.as_str()? {
                    "all" => SessionFilterMode::All,
                    "active" => SessionFilterMode::Active,
                    "running" => SessionFilterMode::Running,
                    _ => return None,
                };
                *self.session_filter.lock().unwrap() = Some(filter);
                Some(self.snapshot_json())
            }
            "toggle-worktree-group" => {
                let key = command.get("key")?.as_str()?.to_string();
                let mut collapsed = self.collapsed_worktree_groups.lock().unwrap();
                if !collapsed.insert(key) {
                    collapsed.remove(command.get("key")?.as_str()?);
                }
                drop(collapsed);
                Some(self.snapshot_json())
            }
            "focus-agent-pane" => {
                let session = command.get("session")?.as_str()?;
                let agent = command.get("agent")?.as_str()?;
                let thread_id = command.get("threadId").and_then(Value::as_str);
                let thread_name = command.get("threadName").and_then(Value::as_str);
                let pane_id = command.get("paneId").and_then(Value::as_str);
                if let Some((provider, pane_id)) =
                    self.resolve_agent_pane(session, agent, thread_id, thread_name, pane_id)
                {
                    self.agent_tracker.lock().unwrap().mark_agent_seen(
                        session,
                        agent,
                        thread_id,
                        Some(&pane_id),
                    );
                    let tty = command
                        .get("clientTty")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| context.and_then(|context| context.client_tty.clone()));
                    if self.should_focus_agent_pane(session, &pane_id) {
                        provider.reveal_pane(&pane_id, tty.as_deref());
                        // Remote agent: also pick its window/pane inside the host's tmux.
                        let remote = thread_id.and_then(|thread| {
                            self.remote_inventory
                                .lock()
                                .unwrap()
                                .remote_targets
                                .get(thread)
                                .cloned()
                        });
                        if let Some((host, remote_pane)) = remote {
                            remote_agents::select_remote_pane(&host, &remote_pane);
                            // Optimistic: the next poll confirms the remote selection.
                            if let Some(thread) = thread_id {
                                self.remote_inventory
                                    .lock()
                                    .unwrap()
                                    .active_thread_by_proxy
                                    .insert(pane_id.clone(), thread.to_string());
                            }
                        }
                    }
                    self.remember_agent_focus(tty.clone(), session, &pane_id);
                    let thread = self.remote_active_thread(&pane_id);
                    return Some(activate_agent_pane_json(
                        session.to_string(),
                        &pane_id,
                        tty.as_deref(),
                        thread.as_deref(),
                    ));
                }
                None
            }
            "kill-agent-pane" => {
                let session = command.get("session")?.as_str()?;
                let agent = command.get("agent")?.as_str()?;
                let thread_id = command.get("threadId").and_then(Value::as_str);
                let thread_name = command.get("threadName").and_then(Value::as_str);
                let pane_id = command.get("paneId").and_then(Value::as_str);
                if let Some((provider, pane_id)) =
                    self.resolve_agent_pane(session, agent, thread_id, thread_name, pane_id)
                {
                    // A remote agent's pane is the local ssh session: never kill it.
                    if self.remote_inventory.lock().unwrap().proxy_panes.contains(&pane_id) {
                        return None;
                    }
                    provider.kill_pane(&pane_id);
                }
                None
            }
            "dismiss-agent" => {
                let session = command.get("session")?.as_str()?;
                let agent = command.get("agent")?.as_str()?;
                let thread_id = command.get("threadId").and_then(Value::as_str);
                let changed = self
                    .agent_tracker
                    .lock()
                    .unwrap()
                    .dismiss(session, agent, thread_id);
                changed.then(|| self.snapshot_json())
            }
            _ => None,
        }
    }

    fn handle_sender_command(&self, command: &Value) -> Option<String> {
        self.handle_sender_command_with_context(command, &mut ClientConnectionContext::default())
    }

    fn handle_sender_command_with_context(
        &self,
        command: &Value,
        context: &mut ClientConnectionContext,
    ) -> Option<String> {
        if command.get("type").and_then(Value::as_str)? != "identify-pane" {
            return None;
        }
        let session_name = command.get("sessionName")?.as_str()?;
        if session_name == "_os_stash" {
            return None;
        }
        context.pane_id = command
            .get("paneId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        context.session_name = Some(session_name.to_string());
        context.window_id = command
            .get("windowId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        debug_log(format!(
            "identify-pane session={:?} pane={:?} window={:?} -> acknowledge_sidebar_connected",
            context.session_name, context.pane_id, context.window_id,
        ));
        self.sidebar_coordinator
            .lock()
            .unwrap()
            .acknowledge_sidebar_connected();
        if let Some(window_id) = context.window_id.as_deref() {
            for provider in &self.providers {
                provider.prepare_sidebar_window(window_id);
            }
        }
        if self.is_sidebar_visible() {
            let width = self.current_sidebar_width_u16();
            if !self.repair_context_sidebar_width(Some(context), width) {
                self.enforce_sidebar_width(width);
            }
        }
        let client_tty = self.providers.first()?.get_client_tty();
        context.client_tty = Some(client_tty.clone());
        Some(format!(
            r#"{{"type":"your-session","name":{},"clientTty":{}}}"#,
            json_string_or_null(Some(session_name)),
            json_string_or_null(Some(&client_tty)),
        ))
    }

    fn handle_http_json(&self, path: &str, body: &Value) -> Option<String> {
        match path {
            "/set-status" => {
                let session = body.get("session")?.as_str()?;
                let tone = body
                    .get("tone")
                    .and_then(Value::as_str)
                    .and_then(parse_metadata_tone);
                match body.get("text") {
                    Some(Value::String(text)) => self
                        .metadata_store
                        .lock()
                        .unwrap()
                        .set_status(session, Some((text.clone(), tone))),
                    Some(Value::Null) | None => self
                        .metadata_store
                        .lock()
                        .unwrap()
                        .set_status(session, None),
                    _ => return None,
                }
            }
            "/set-progress" => {
                let session = body.get("session")?.as_str()?;
                if body.get("clear").and_then(Value::as_bool).unwrap_or(false) {
                    self.metadata_store
                        .lock()
                        .unwrap()
                        .set_progress(session, None);
                } else {
                    self.metadata_store.lock().unwrap().set_progress(
                        session,
                        Some((
                            body.get("current").and_then(Value::as_u64),
                            body.get("total").and_then(Value::as_u64),
                            body.get("percent").and_then(Value::as_f64),
                            body.get("label")
                                .and_then(Value::as_str)
                                .map(ToString::to_string),
                        )),
                    );
                }
            }
            "/log" | "/notify" => {
                let session = body.get("session")?.as_str()?;
                let message = body.get("message")?.as_str()?.to_string();
                let tone = body
                    .get("tone")
                    .and_then(Value::as_str)
                    .and_then(parse_metadata_tone);
                let source = body
                    .get("source")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                self.metadata_store
                    .lock()
                    .unwrap()
                    .append_log(session, message, tone, source);
            }
            "/clear-log" => {
                let session = body.get("session")?.as_str()?;
                self.metadata_store.lock().unwrap().clear_logs(session);
            }
            _ => return None,
        }
        Some(self.snapshot_json())
    }

    fn handle_agent_event_json(&self, body: &Value) -> Result<String, AgentEventError> {
        self.apply_agent_event(body)?;
        Ok(self.snapshot_json())
    }

    fn agent_list_json(&self) -> Result<Value, AgentNameError> {
        let agents = self.cached_live_agents()?;
        let mut names = self.agent_names.lock().unwrap();
        names.observe(&agents)?;
        Ok(names.list(&agents))
    }

    fn rename_agent_json(&self, body: &Value) -> Result<(Value, Option<String>), AgentNameError> {
        let agents = self.cached_live_agents()?;
        let cached = self.cached_snapshot.lock().unwrap().clone();
        let mut names = self.agent_names.lock().unwrap();
        names.observe(&agents)?;
        let result = names.rename(&agents, body)?;
        let snapshot = cached.map(|mut state| {
            names.overlay_state(&mut state);
            serde_json::to_string(&ServerMessage::State(state)).expect("state must serialize")
        });
        Ok((result, snapshot))
    }

    fn handle_pi_runtime_upsert(&self, body: &Value) -> Result<(), PiRuntimeError> {
        let info =
            parse_pi_runtime_info(body, (self.now_ms)()).ok_or(PiRuntimeError::InvalidPayload)?;
        self.pi_runtime_registry.lock().unwrap().upsert(info);
        Ok(())
    }

    fn handle_pi_runtime_delete(&self, body: &Value) -> Result<(), PiRuntimeError> {
        let pid = body
            .get("pid")
            .and_then(Value::as_u64)
            .filter(|pid| *pid > 0 && *pid <= u32::MAX as u64)
            .ok_or(PiRuntimeError::MissingPid)? as u32;
        self.pi_runtime_registry.lock().unwrap().delete(pid);
        Ok(())
    }

    fn handle_http_text(&self, path: &str, body: &str) -> Option<String> {
        if path != "/focus" {
            return None;
        }
        let context = parse_context(body)?;
        if context.pane_active == Some(false) {
            return None;
        }
        let pane = context.pane_id.as_deref()?;
        let focus = (context.session.clone(), pane.to_string());
        if self
            .last_focus_hook_by_client
            .lock()
            .unwrap()
            .insert(context.client_tty.clone(), focus.clone())
            .as_ref()
            == Some(&focus)
        {
            return None;
        }
        let name = context.session.clone();
        *self.focused_session.lock().unwrap() = Some(name.clone());
        self.remember_focused_pane(&context);
        if self.is_agent_focus_pane(&name, pane) {
            self.agent_focus_by_client
                .lock()
                .unwrap()
                .insert(context.client_tty.clone(), focus);
            let thread = self.remote_active_thread(pane);
            return Some(activate_agent_pane_json(
                name,
                pane,
                context.client_tty.as_deref(),
                thread.as_deref(),
            ));
        }
        None
    }

    fn handle_http_hook(&self, path: &str, body: &str) -> Option<String> {
        match path {
            "/toggle" => {
                self.toggle_sidebar();
                Some(self.snapshot_json())
            }
            "/ensure-sidebar" => {
                let spawned = self.ensure_sidebar(body);
                parse_context_session(body)
                    .map(|name| activate_session_json(name, None))
                    .or_else(|| spawned.then(|| self.snapshot_json()))
            }
            "/pane-exited" => {
                let fallback_sessions = self
                    .providers
                    .iter()
                    .flat_map(|provider| provider.list_sidebar_panes(None))
                    .filter_map(|pane| {
                        let fallback = self
                            .session_before(&pane.session_name)
                            .or_else(|| self.session_after(&pane.session_name))?;
                        Some((pane.session_name, fallback))
                    })
                    .collect::<HashMap<_, _>>();
                for provider in &self.providers {
                    provider.kill_orphaned_sidebar_panes_with_fallbacks(&fallback_sessions);
                }
                if self.is_sidebar_visible() {
                    self.enforce_sidebar_width(self.current_sidebar_width_u16());
                }
                None
            }
            "/pane-layout-changed" | "/client-resized" => {
                if self.is_sidebar_visible() {
                    self.enforce_sidebar_width(self.current_sidebar_width_u16());
                }
                None
            }
            _ => None,
        }
    }

    fn handle_switch_index(&self, index: u32, body: &str) -> Option<String> {
        let client_tty = parse_context(body).and_then(|context| context.client_tty);
        self.switch_visible_index(index, client_tty.as_deref())
    }
}

impl ReadOnlyMuxStateSource {
    fn should_focus_agent_pane(&self, session: &str, pane: &str) -> bool {
        let mut last = self.last_explicit_agent_focus.lock().unwrap();
        let repeated = last
            .as_ref()
            .is_some_and(|(previous_session, previous_pane, at)| {
                previous_session == session
                    && previous_pane == pane
                    && at.elapsed() < std::time::Duration::from_millis(AGENT_FOCUS_COOLDOWN_MS)
            });
        if repeated {
            return false;
        }
        *last = Some((session.to_string(), pane.to_string(), Instant::now()));
        true
    }

    /// Poll remote hosts and remap their agents onto local ssh panes.
    /// Reports whether rows changed and which highlight events to send.
    fn refresh_remote_agents(&self) -> RemoteRefresh {
        let hosts = self.remote_hosts.clone();
        let ttys = remote_agents::local_ssh_ttys(&hosts);
        let mut proxies: HashMap<String, Vec<remote_agents::LocalSshPane>> = HashMap::new();
        if !ttys.is_empty() {
            for provider in &self.providers {
                for (tty, pane_id, session) in provider.pane_ttys() {
                    if let Some(host) = ttys.get(&tty) {
                        proxies
                            .entry(host.clone())
                            .or_default()
                            .push(remote_agents::LocalSshPane { pane_id, session });
                    }
                }
            }
        }
        // Only hosts with a local ssh pane are worth an SSH round-trip.
        let handles = hosts
            .into_iter()
            .filter(|host| proxies.contains_key(host))
            .map(|host| {
                std::thread::spawn(move || {
                    let bindings = remote_agents::fetch_host(&host);
                    (host, bindings)
                })
            })
            .collect::<Vec<_>>();
        let bindings = handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .filter_map(|(host, bindings)| bindings.map(|bindings| (host, bindings)))
            .collect::<HashMap<_, _>>();
        let inventory = remote_agents::build_inventory(&bindings, &proxies, current_time_ms());
        let mut current = self.remote_inventory.lock().unwrap();
        let comparable = |inventory: &remote_agents::RemoteInventory| {
            let mut rows = inventory
                .agents_by_session
                .values()
                .flatten()
                .map(|agent| {
                    (
                        agent.thread_id.clone(),
                        agent.status,
                        agent.thread_name.clone(),
                        agent.location.clone(),
                        agent.session.clone(),
                    )
                })
                .collect::<Vec<_>>();
            rows.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
            rows
        };
        let changed = comparable(&current) != comparable(&inventory)
            || current.proxy_panes != inventory.proxy_panes;
        let moved = inventory
            .active_thread_by_proxy
            .iter()
            .filter(|(pane, thread)| current.active_thread_by_proxy.get(*pane) != Some(*thread))
            .map(|(pane, thread)| (pane.clone(), thread.clone()))
            .collect::<Vec<_>>();
        *current = inventory;
        drop(current);
        // Clients looking at a proxy pane follow the host's active agent.
        let focus = self.agent_focus_by_client.lock().unwrap().clone();
        let activations = moved
            .iter()
            .flat_map(|(pane, thread)| {
                focus
                    .iter()
                    .filter(move |(_, (_, focused))| focused == pane)
                    .map(move |(tty, (session, _))| {
                        activate_agent_pane_json(session.clone(), pane, tty.as_deref(), Some(thread))
                    })
            })
            .collect();
        RemoteRefresh {
            changed,
            activations,
        }
    }

    fn cached_live_agents(&self) -> Result<Vec<AgentEvent>, AgentNameError> {
        if !self.confirmed_inventory_ready.load(Ordering::Acquire) {
            return Err(AgentNameError::unavailable());
        }
        let tracker = self.agent_tracker.lock().unwrap();
        let panes = self.confirmed_agent_panes.lock().unwrap();
        let mut agents = panes
            .iter()
            .flat_map(|(session, panes)| {
                agents_with_live_panes(session, tracker.get_agents(session), panes)
            })
            .filter(|agent| agent.liveness == Some(AgentLiveness::Alive))
            .collect::<Vec<_>>();
        agents.extend(
            self.remote_inventory
                .lock()
                .unwrap()
                .agents_by_session
                .values()
                .flatten()
                .cloned(),
        );
        agents.sort_by(|left, right| {
            (&left.session, &left.pane_id).cmp(&(&right.session, &right.pane_id))
        });
        Ok(agents)
    }

    fn apply_agent_event(&self, body: &Value) -> Result<(), AgentEventError> {
        let agent = body
            .get("agent")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or(AgentEventError::MissingAgent)?;
        let status = body
            .get("status")
            .and_then(Value::as_str)
            .and_then(parse_agent_status)
            .ok_or(AgentEventError::InvalidStatus)?;
        let session = self
            .resolve_agent_event_session(body)
            .ok_or(AgentEventError::CouldNotResolveSession)?;
        let ts = body
            .get("ts")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| (self.now_ms)());
        let pane_id = body
            .get("paneId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let event_pane_id = pane_id.clone();
        let event_session = session.clone();
        self.agent_tracker.lock().unwrap().apply_event(AgentEvent {
            agent,
            session,
            status,
            ts,
            thread_id: body
                .get("threadId")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            thread_name: body
                .get("threadName")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            last_user_prompt: body
                .get("lastUserPrompt")
                .or_else(|| body.get("last_user_prompt"))
                .and_then(Value::as_str)
                .map(ToString::to_string),
            unseen: None,
            liveness: pane_id.as_ref().map(|_| AgentLiveness::Alive),
            pane_id,
            location: None,
        });
        if let Some(pane_id) = event_pane_id
            && self
                .focused_pane_by_session
                .lock()
                .unwrap()
                .get(&event_session)
                .is_some_and(|focused_pane| focused_pane == &pane_id)
        {
            debug_log(format!(
                "agent-event-focused-pane session={} pane={} -> mark seen",
                event_session, pane_id,
            ));
            self.agent_tracker
                .lock()
                .unwrap()
                .mark_pane_seen(&event_session, &pane_id);
        }
        Ok(())
    }

    fn apply_agent_watcher_snapshot(&self, snapshot: AgentWatcherSnapshot) -> bool {
        if snapshot.status == AgentStatus::Idle {
            debug_log(format!(
                "watcher-snapshot ignored idle agent={} thread_id={:?} thread_name={:?} project_dir={:?}",
                snapshot.agent, snapshot.thread_id, snapshot.thread_name, snapshot.project_dir,
            ));
            return false;
        }
        let Some(session) = self.resolve_agent_watcher_session(&snapshot) else {
            debug_log(format!(
                "watcher-snapshot unresolved agent={} status={:?} thread_id={:?} thread_name={:?} project_dir={:?}",
                snapshot.agent,
                snapshot.status,
                snapshot.thread_id,
                snapshot.thread_name,
                snapshot.project_dir,
            ));
            return false;
        };
        let Some(provider) = self.provider_for_session(&session) else {
            return false;
        };
        let Some(pane_id) = provider.resolve_agent_pane_id(
            &session,
            snapshot.agent,
            snapshot.thread_id.as_deref(),
            snapshot.thread_name.as_deref(),
        ) else {
            return false;
        };
        if !provider
            .list_agent_panes(&session)
            .iter()
            .any(|pane| pane.pane_id == pane_id && pane.agent == snapshot.agent)
        {
            return false;
        }
        let focused_pane = self
            .focused_pane_by_session
            .lock()
            .unwrap()
            .get(&session)
            .cloned();
        debug_log(format!(
            "watcher-snapshot applying session={} focused_pane={:?} agent={} status={:?} thread_id={:?} thread_name={:?} project_dir={:?}",
            session,
            focused_pane,
            snapshot.agent,
            snapshot.status,
            snapshot.thread_id,
            snapshot.thread_name,
            snapshot.project_dir,
        ));
        let event = AgentEvent {
            agent: snapshot.agent.to_string(),
            session: session.clone(),
            status: snapshot.status,
            ts: snapshot.ts,
            thread_id: snapshot.thread_id.clone(),
            thread_name: snapshot.thread_name.clone(),
            last_user_prompt: snapshot.last_user_prompt.clone(),
            unseen: None,
            pane_id: Some(pane_id),
            liveness: Some(AgentLiveness::Alive),
            location: None,
        };
        self.agent_tracker.lock().unwrap().apply_event(event);
        if let Some(pane_id) = focused_pane {
            let changed = self
                .agent_tracker
                .lock()
                .unwrap()
                .mark_pane_seen(&session, &pane_id);
            debug_log(format!(
                "watcher-snapshot-focused-pane-seen session={} pane={} agent={} thread_id={:?} thread_name={:?} changed={changed}",
                session, pane_id, snapshot.agent, snapshot.thread_id, snapshot.thread_name,
            ));
        }
        true
    }

    fn resolve_agent_watcher_session(&self, snapshot: &AgentWatcherSnapshot) -> Option<String> {
        let sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .collect::<Vec<_>>();
        let project_dir = snapshot.project_dir.as_deref()?;

        if let Some(encoded) = project_dir.strip_prefix("__encoded__:") {
            return sessions
                .iter()
                .find(|session| encode_agent_project_dir(&session.dir) == encoded)
                .map(|session| session.name.clone());
        }

        let dir_session_map = build_dir_session_map(
            sessions
                .into_iter()
                .map(|session| (session.name, session.dir)),
        );
        resolve_session_for_project_dir(project_dir, &dir_session_map)
    }

    fn resolve_agent_event_session(&self, body: &Value) -> Option<String> {
        let sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .collect::<Vec<_>>();

        if let Some(project_dir) = body.get("projectDir").and_then(Value::as_str) {
            let dir_session_map = build_dir_session_map(
                sessions
                    .iter()
                    .map(|session| (session.name.clone(), session.dir.clone())),
            );
            if let Some(session) = resolve_session_for_project_dir(project_dir, &dir_session_map) {
                return Some(session);
            }
        }

        body.get("tmuxSession")
            .and_then(Value::as_str)
            .filter(|tmux_session| sessions.iter().any(|session| session.name == *tmux_session))
            .map(ToString::to_string)
    }

    fn resolve_agent_pane(
        &self,
        session: &str,
        agent: &str,
        thread_id: Option<&str>,
        thread_name: Option<&str>,
        pane_id: Option<&str>,
    ) -> Option<(Arc<dyn MuxProvider>, String)> {
        let provider = self.provider_for_session(session)?;
        if let Some(pane_id) = pane_id {
            let confirmed = self.cached_live_agents().ok()?.into_iter().any(|live| {
                live.session == session
                    && live.agent == agent
                    && live.pane_id.as_deref() == Some(pane_id)
                    && thread_id.is_none_or(|thread| live.thread_id.as_deref() == Some(thread))
            });
            return confirmed.then(|| (provider, pane_id.to_string()));
        }
        self.sync_agent_pane_presence();
        if let Some(pane_id) = self.resolve_tracked_agent_pane(session, agent, thread_id) {
            return Some((provider, pane_id));
        }
        let pane_id = provider.resolve_agent_pane_id(session, agent, thread_id, thread_name)?;
        Some((provider, pane_id))
    }

    fn resolve_tracked_agent_pane(
        &self,
        session: &str,
        agent: &str,
        thread_id: Option<&str>,
    ) -> Option<String> {
        let thread_id = thread_id?;
        self.agent_tracker
            .lock()
            .unwrap()
            .get_agents(session)
            .into_iter()
            .find(|event| {
                event.agent == agent
                    && event.thread_id.as_deref() == Some(thread_id)
                    && event.liveness == Some(AgentLiveness::Alive)
                    && event.pane_id.is_some()
            })
            .and_then(|event| event.pane_id)
    }

    fn sidebar_panes_to_resize(&self, width: u16) -> Vec<String> {
        let mut pane_ids = Vec::new();
        for provider in &self.providers {
            if !provider.is_sidebar_capable() {
                continue;
            }
            for pane in provider.list_sidebar_panes(None) {
                if pane.width == Some(width) {
                    continue;
                }
                pane_ids.push(pane.pane_id);
            }
        }
        pane_ids.reverse();
        pane_ids
    }

    fn repair_context_sidebar_width(
        &self,
        context: Option<&ClientConnectionContext>,
        width: u16,
    ) -> bool {
        let Some(pane_id) = context.and_then(|context| context.pane_id.as_deref()) else {
            return false;
        };
        debug_log(format!(
            "width-repair: resize context pane={pane_id} to={width}"
        ));
        for provider in &self.providers {
            provider.resize_sidebar_pane(pane_id, width);
        }
        true
    }

    fn enforce_sidebar_width(&self, width: u16) {
        let panes = self.sidebar_panes_to_resize(width);
        for pane_id in panes {
            debug_log(format!("width-repair: resize pane={pane_id} to={width}",));
            for provider in &self.providers {
                provider.resize_sidebar_pane(&pane_id, width);
            }
        }
    }

    fn provider_for_session(&self, session: &str) -> Option<Arc<dyn MuxProvider>> {
        self.providers
            .iter()
            .find(|provider| {
                provider
                    .list_sessions()
                    .iter()
                    .any(|mux_session| mux_session.name == session)
            })
            .cloned()
            .or_else(|| self.providers.first().cloned())
    }

    fn git_info_by_session(
        &self,
        visible_session_names: Option<&[String]>,
    ) -> Option<HashMap<String, GitInfo>> {
        let visible =
            visible_session_names.map(|names| names.iter().cloned().collect::<HashSet<_>>());
        let mut git_by_session = HashMap::new();
        for provider in &self.providers {
            for session in provider.list_sessions() {
                if visible
                    .as_ref()
                    .is_some_and(|visible| !visible.contains(&session.name))
                {
                    continue;
                }
                git_by_session.insert(session.name, self.git_info_for_dir(&session.dir));
            }
        }
        Some(git_by_session)
    }

    fn git_info_for_dir(&self, dir: &str) -> GitInfo {
        if dir.is_empty() {
            return GitInfo::empty();
        }

        let now = (self.now_ms)();
        if let Some(cached) = self.git_info_cache.lock().unwrap().get(dir).cloned()
            && now.saturating_sub(cached.ts) < GIT_CACHE_TTL_MS
        {
            return cached.info;
        }

        let output = self.git_command_runner.git_info_output(dir);
        if output.is_empty() {
            return GitInfo::empty();
        }
        let info = parse_git_info_output(&output);
        self.git_info_cache.lock().unwrap().insert(
            dir.to_string(),
            CachedGitInfo {
                info: info.clone(),
                ts: now,
            },
        );
        info
    }

    fn discover_live_ports(
        &self,
        visible_session_names: Option<&[String]>,
    ) -> Option<HashMap<String, Vec<u16>>> {
        let session_names = visible_session_names
            .map(|names| names.to_vec())
            .unwrap_or_else(|| self.sorted_session_names());
        let now = (self.now_ms)();
        if let Some(cached) = self.port_snapshot_cache.lock().unwrap().clone()
            && cached.session_names == session_names
            && now.saturating_sub(cached.ts) < PORT_POLL_INTERVAL_MS
        {
            return Some(cached.ports_by_session);
        }

        if session_names.is_empty() {
            return Some(HashMap::new());
        }

        let session_filter = session_names.iter().cloned().collect::<HashSet<_>>();
        let mut pane_pids_by_session = HashMap::new();
        for provider in &self.providers {
            for session in provider.list_sessions() {
                if !session_filter.contains(&session.name) {
                    continue;
                }
                let pids = provider.get_session_pane_pids(&session.name);
                if !pids.is_empty() {
                    pane_pids_by_session.insert(session.name, pids);
                }
            }
        }

        if pane_pids_by_session.is_empty() {
            return Some(discover_session_ports(PortDiscoveryInput {
                session_names,
                pane_pids_by_session,
                process_rows: Vec::new(),
                lsof_fields: "",
            }));
        }

        let lsof_fields = self.port_command_runner.lsof_fields();
        let cache_session_names = session_names.clone();
        let ports_by_session = discover_session_ports(PortDiscoveryInput {
            session_names,
            pane_pids_by_session,
            process_rows: self.port_command_runner.process_rows(),
            lsof_fields: &lsof_fields,
        });
        self.port_snapshot_cache
            .lock()
            .unwrap()
            .replace(CachedPortSnapshot {
                session_names: cache_session_names,
                ports_by_session: ports_by_session.clone(),
                ts: now,
            });
        Some(ports_by_session)
    }

    fn toggle_sidebar(&self) {
        let providers = self
            .providers
            .iter()
            .filter(|provider| provider.is_full_sidebar_capable())
            .collect::<Vec<_>>();
        let panes_by_provider = providers
            .iter()
            .map(|provider| (*provider, provider.list_sidebar_panes(None)))
            .collect::<Vec<_>>();

        if panes_by_provider.iter().any(|(_, panes)| !panes.is_empty()) {
            for (provider, panes) in panes_by_provider {
                for pane in panes {
                    provider.hide_sidebar(&pane.pane_id);
                }
            }
            self.sidebar_coordinator.lock().unwrap().hide();
            return;
        }

        let warmup_until = (self.now_ms)().saturating_add(SIDEBAR_WARMUP_MS);
        self.sidebar_coordinator
            .lock()
            .unwrap()
            .begin_warmup_until(warmup_until);
        let width = self.current_sidebar_width_u16();
        for provider in providers {
            let mut unique_windows = Vec::<ActiveWindow>::new();
            for window in provider.list_active_windows() {
                if let Some(current) = unique_windows
                    .iter_mut()
                    .find(|current| current.id == window.id)
                {
                    if !current.active && window.active {
                        *current = window;
                    } else {
                        debug_log(format!(
                            "toggle_sidebar: skipping duplicate linked window session={} window={}",
                            window.session_name, window.id,
                        ));
                    }
                    continue;
                }
                unique_windows.push(window);
            }

            for window in unique_windows {
                debug_log(format!(
                    "toggle_sidebar: spawning in session={} window={} width={width}",
                    window.session_name, window.id,
                ));
                provider.spawn_sidebar(
                    &window.session_name,
                    &window.id,
                    width,
                    SidebarPosition::Left,
                    SIDEBAR_SCRIPTS_DIR,
                );
            }
        }
    }

    fn ensure_sidebar(&self, body: &str) -> bool {
        let context = parse_context(body);
        let width = self.current_sidebar_width_u16();
        if !self.is_sidebar_visible() {
            debug_log("ensure_sidebar: ignored spawn while sidebar is hidden");
            return false;
        }
        // A window switch / new window can make tmux proportionally redistribute
        // panes in that window, so repair existing sidebars before spawning any
        // missing ones. This is event-driven, not a per-tick width scan.
        self.enforce_sidebar_width(width);
        let mut spawned = false;
        for provider in &self.providers {
            if !provider.is_full_sidebar_capable() {
                continue;
            }
            let session_name = context
                .as_ref()
                .map(|context| context.session.clone())
                .or_else(|| provider.get_current_session());
            let window_id = context
                .as_ref()
                .map(|context| context.window_id.clone())
                .or_else(|| provider.get_current_window_id());
            let (Some(session_name), Some(window_id)) = (session_name, window_id) else {
                continue;
            };
            if provider
                .list_sidebar_panes(Some(&session_name))
                .iter()
                .any(|pane| pane.window_id == window_id)
            {
                continue;
            }
            let warmup_until = (self.now_ms)().saturating_add(SIDEBAR_WARMUP_MS);
            self.sidebar_coordinator
                .lock()
                .unwrap()
                .begin_warmup_until(warmup_until);
            provider.spawn_sidebar(
                &session_name,
                &window_id,
                width,
                SidebarPosition::Left,
                SIDEBAR_SCRIPTS_DIR,
            );
            spawned = true;
        }
        spawned
    }

    fn switch_visible_index(&self, index: u32, client_tty: Option<&str>) -> Option<String> {
        let provider = self.providers.first()?;
        let target_index = index.checked_sub(1).map(|index| index as usize)?;
        let name = self
            .sidebar_display_session_names()
            .and_then(|names| names.get(target_index).cloned())?;
        provider.switch_session(&name, client_tty);
        None
    }

    fn session_before(&self, name: &str) -> Option<String> {
        let names = self.sidebar_display_session_names()?;
        let index = names.iter().position(|candidate| candidate == name)?;
        index
            .checked_sub(1)
            .and_then(|previous| names.get(previous).cloned())
    }

    fn session_after(&self, name: &str) -> Option<String> {
        let names = self.sidebar_display_session_names()?;
        let index = names.iter().position(|candidate| candidate == name)?;
        names.get(index + 1).cloned()
    }

    fn sidebar_display_session_names(&self) -> Option<Vec<String>> {
        app_from_state_json(&self.snapshot_json()).map(|app| {
            app.display_sessions()
                .into_iter()
                .map(|session| session.name.clone())
                .collect()
        })
    }

    fn sidebar_reordered_session_names(&self, name: &str, delta: i8) -> Option<Vec<String>> {
        app_from_state_json(&self.snapshot_json())?.reordered_session_names(name, delta)
    }

    fn sidebar_reordered_worktree_group_names(&self, key: &str, delta: i8) -> Option<Vec<String>> {
        app_from_state_json(&self.snapshot_json())?.reordered_worktree_group_names(key, delta)
    }

    fn visible_session_names(&self) -> Option<Vec<String>> {
        let names = self.sorted_session_names();
        let mut session_order = self.session_order.lock().unwrap();
        session_order.sync(names.clone());
        if let Some(current_session) = self
            .providers
            .iter()
            .find_map(|provider| provider.get_current_session())
        {
            session_order.show(&current_session);
        }
        Some(session_order.apply(names))
    }

    fn sorted_session_names(&self) -> Vec<String> {
        let mut sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        sessions.into_iter().map(|session| session.name).collect()
    }
}

/// Background ticker that advances sidebar lifecycle timers. This keeps
/// user-visible lifecycle states like `warming up…` stable long enough to be
/// perceived, then broadcasts the transition back to ready without relying on
/// unrelated tmux or websocket traffic.
async fn run_sidebar_lifecycle_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = interval.tick() => {
                let now = (source.now_ms)();
                let changed = {
                    let mut coordinator = source.sidebar_coordinator.lock().unwrap();
                    coordinator.tick_timers(now)
                };
                if changed {
                    debug_log("sidebar_lifecycle_loop: lifecycle changed, broadcasting fresh state");
                    let worker_source = source.clone();
                    if let Ok(snapshot) = tokio::task::spawn_blocking(move || worker_source.snapshot_json()).await {
                        let _ = state_updates.send(snapshot);
                    }
                }
            }
        }
    }
}

/// Poll tmux state on a fixed cadence and broadcast a fresh snapshot whenever
/// the JSON differs from the last broadcast, so the sidebar picks up new
/// sessions, agent panes, focus changes, and other mux state without requiring
/// an explicit hook.
struct RemoteRefresh {
    changed: bool,
    activations: Vec<String>,
}

/// Best-effort remote agent discovery; unreachable hosts are skipped.
async fn run_remote_agent_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    if source.remote_hosts.is_empty() {
        return;
    }
    let mut shutdown_rx = shutdown.subscribe();
    let mut interval =
        tokio::time::interval(Duration::from_secs(remote_agents::REMOTE_POLL_SECS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = interval.tick() => {
                let worker = source.clone();
                let Ok(refresh) = tokio::task::spawn_blocking(move || worker.refresh_remote_agents()).await else {
                    continue;
                };
                if refresh.changed {
                    let worker = source.clone();
                    if let Ok(snapshot) = tokio::task::spawn_blocking(move || worker.snapshot_json()).await {
                        let _ = state_updates.send(snapshot);
                    }
                }
                for activation in refresh.activations {
                    let _ = state_updates.send(activation);
                }
            }
        }
    }
}

async fn run_tmux_state_poll_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut shutdown_rx = shutdown.subscribe();
    let mut interval = tokio::time::interval(Duration::from_millis(TMUX_STATE_POLL_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_hash = None;
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = interval.tick() => {
                // Hooks correct tmux layout churn immediately; this slower poll
                // is only a backstop for missed external tmux changes.

                let worker_source = source.clone();
                let Ok(snapshot) = tokio::task::spawn_blocking(move || worker_source.snapshot_json()).await else {
                    continue;
                };
                // Hash the snapshot ignoring the per-tick `ts` field so that
                // identical state on consecutive ticks does not trigger a
                // wasteful re-broadcast. Anything else changing (sessions,
                // panes, widths, init state, focus) flips the hash and the
                // sidebar receives a fresh state.
                let stripped = strip_ts_field(&snapshot);
                let mut hasher = DefaultHasher::new();
                stripped.hash(&mut hasher);
                let hash = hasher.finish();
                if Some(hash) != last_hash {
                    last_hash = Some(hash);
                    debug_log("tmux_state_poll_loop: state changed, broadcasting");
                    let _ = state_updates.send(snapshot);
                }
            }
        }
    }
}

/// Remove `,"ts":\d+` (or leading variant) from a JSON snapshot string so a
/// monotonically increasing timestamp does not defeat the change-detection
/// hash in `run_tmux_state_poll_loop`. Cheap byte scan; no full JSON parse.
fn strip_ts_field(snapshot: &str) -> String {
    let mut out = String::with_capacity(snapshot.len());
    let bytes = snapshot.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &snapshot[i..];
        let key = "\"ts\":";
        if rest.starts_with(key)
            || rest.starts_with(&format!(",{key}"))
            || rest.starts_with(&format!("{{{key}"))
        {
            // Preserve a leading `,` or `{` while dropping the rest of the
            // `"ts":<digits>` token.
            let mut prefix_len = 0;
            if rest.starts_with(',') || rest.starts_with('{') {
                prefix_len = 1;
                out.push(rest.chars().next().unwrap());
            }
            // Skip past `"ts":`
            let mut j = i + prefix_len + key.len();
            // Skip digits.
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            // If we left a leading `,`, also drop a trailing `,` to avoid
            // doubling separators when ts was sandwiched.
            if prefix_len == 1 && bytes.get(i) == Some(&b',') && bytes.get(j) == Some(&b',') {
                j += 1;
            }
            i = j;
            continue;
        }
        out.push(snapshot[i..].chars().next().unwrap());
        i += snapshot[i..].chars().next().unwrap().len_utf8();
    }
    out
}

async fn run_agent_watcher_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let mut interval = tokio::time::interval(Duration::from_millis(AGENT_WATCHER_POLL_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_seen = HashMap::<String, AgentWatcherFingerprint>::new();

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = interval.tick() => {
                let now = current_time_ms();
                let snapshots = tokio::task::spawn_blocking(move || scan_agent_watcher_snapshots(now))
                    .await
                    .unwrap_or_default();
                debug_log(format!(
                    "agent_watcher_loop: tick scanned {} snapshots",
                    snapshots.len()
                ));
                for snapshot in snapshots {
                    if snapshot.status == AgentStatus::Idle {
                        continue;
                    }
                    let key = agent_watcher_key(&snapshot);
                    let fingerprint = AgentWatcherFingerprint::from(&snapshot);
                    if last_seen.get(&key) == Some(&fingerprint) {
                        continue;
                    }
                    let agent = snapshot.agent.to_string();
                    let status = snapshot.status;
                    let thread_name = snapshot.thread_name.clone();
                    let worker_source = source.clone();
                    let applied = tokio::task::spawn_blocking(move || {
                        worker_source.apply_agent_watcher_snapshot(snapshot).then(|| worker_source.snapshot_json())
                    }).await.unwrap_or_default();
                    if let Some(payload) = applied {
                        debug_log(format!(
                            "agent_watcher_loop: applied snapshot agent={agent} status={status:?} thread={thread_name:?}",
                        ));
                        last_seen.insert(key, fingerprint);
                        let _ = state_updates.send(payload);
                    } else {
                        debug_log(format!(
                            "agent_watcher_loop: dropped snapshot agent={agent} status={status:?} (no matching session)",
                        ));
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentWatcherFingerprint {
    status: AgentStatus,
    thread_name: Option<String>,
    last_user_prompt: Option<String>,
    project_dir: Option<String>,
}

impl From<&AgentWatcherSnapshot> for AgentWatcherFingerprint {
    fn from(snapshot: &AgentWatcherSnapshot) -> Self {
        Self {
            status: snapshot.status,
            thread_name: snapshot.thread_name.clone(),
            last_user_prompt: snapshot.last_user_prompt.clone(),
            project_dir: snapshot.project_dir.clone(),
        }
    }
}

fn agent_watcher_key(snapshot: &AgentWatcherSnapshot) -> String {
    format!(
        "{}\0{}",
        snapshot.agent,
        snapshot
            .thread_id
            .as_deref()
            .or(snapshot.project_dir.as_deref())
            .unwrap_or_default(),
    )
}

fn scan_agent_watcher_snapshots(now_ms: u64) -> Vec<AgentWatcherSnapshot> {
    let mut snapshots = Vec::new();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return snapshots;
    };

    scan_amp_threads(&home, now_ms, &mut snapshots);
    scan_claude_code_projects(&home, now_ms, &mut snapshots);
    scan_codex_sessions(&home, now_ms, &mut snapshots);
    scan_opencode_sessions(&home, now_ms, &mut snapshots);
    scan_pi_sessions(&home, now_ms, &mut snapshots);
    scan_droid_sessions(&home, now_ms, &mut snapshots);
    snapshots
}

fn scan_amp_threads(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let threads_dir = home.join(".local/share/amp/threads");
    let Ok(entries) = fs::read_dir(threads_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(snapshot) = amp_snapshot_from_thread_json(&raw, mtime_ms) {
            snapshots.push(snapshot);
        }
    }
}

fn scan_claude_code_projects(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let projects_dir = home.join(".claude/projects");
    let Ok(projects) = fs::read_dir(projects_dir) else {
        return;
    };

    for project in projects.flatten() {
        let project_path = project.path();
        if !project_path.is_dir() {
            continue;
        }
        let encoded = project.file_name().to_string_lossy().to_string();
        let project_dir = decode_claude_project_dir(&encoded, |path| Path::new(path).is_dir());
        let Ok(files) = fs::read_dir(project_path) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(mtime_ms) = file_mtime_ms(&path) else {
                continue;
            };
            if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
                continue;
            }
            let Some(thread_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            if let Some(snapshot) =
                claude_code_snapshot_from_jsonl(thread_id, &project_dir, &raw, mtime_ms, now_ms)
            {
                snapshots.push(snapshot);
            }
        }
    }
}

fn scan_codex_sessions(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let sessions_dir = codex_home.join("sessions");
    let names = fs::read_to_string(codex_home.join("session_index.jsonl"))
        .ok()
        .map(|raw| {
            parse_codex_session_index(&raw)
                .into_iter()
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    for path in collect_jsonl_files(&sessions_dir) {
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Some(path_text) = path.to_str() else {
            continue;
        };
        let thread_id = codex_thread_id_from_path(path_text);
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(snapshot) = codex_snapshot_from_jsonl(
            &thread_id,
            &raw,
            names.get(&thread_id).map(String::as_str),
            mtime_ms,
            now_ms,
        ) {
            snapshots.push(snapshot);
        }
    }
}

fn scan_opencode_sessions(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let db_path = std::env::var_os("OPENCODE_DB_PATH")
        .or_else(|| std::env::var_os("OPENCODE_DB"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share/opencode/opencode.db"));
    if !db_path.exists() {
        return;
    }

    let stale_threshold = now_ms.saturating_sub(AGENT_WATCHER_RECENT_MS);
    let query = format!(
        "WITH recent AS MATERIALIZED (SELECT id, title, directory, time_updated FROM session WHERE time_updated > {stale_threshold} ORDER BY time_updated DESC LIMIT 50) SELECT r.id, ifnull(r.title,''), r.directory, r.time_updated, ifnull((SELECT m.data FROM message m WHERE m.session_id = r.id ORDER BY m.time_created DESC LIMIT 1),''), ifnull((SELECT sm.data FROM session_message sm WHERE sm.session_id = r.id AND sm.type = 'user' ORDER BY sm.seq DESC LIMIT 1),'') FROM recent r ORDER BY r.time_updated DESC;"
    );
    let run_query = |query: String| {
        let mut command = process::Command::new("sqlite3");
        command
            .arg("-readonly")
            .arg("-separator")
            .arg(OPENCODE_SQL_SEP.to_string())
            .arg(&db_path)
            .arg(query);
        run_process_with_timeout(command, Duration::from_millis(OPENCODE_SQL_TIMEOUT_MS))
    };
    let output = run_query(query).or_else(|| {
        let legacy_query = format!(
            "WITH recent AS MATERIALIZED (SELECT id, title, directory, time_updated FROM session WHERE time_updated > {stale_threshold} ORDER BY time_updated DESC LIMIT 50) SELECT r.id, ifnull(r.title,''), r.directory, r.time_updated, ifnull((SELECT m.data FROM message m WHERE m.session_id = r.id ORDER BY m.time_created DESC LIMIT 1),'') FROM recent r ORDER BY r.time_updated DESC;"
        );
        run_query(legacy_query)
    });
    let Some(mut output) = output else {
        return;
    };
    if !output.status.success() {
        let legacy_query = format!(
            "WITH recent AS MATERIALIZED (SELECT id, title, directory, time_updated FROM session WHERE time_updated > {stale_threshold} ORDER BY time_updated DESC LIMIT 50) SELECT r.id, ifnull(r.title,''), r.directory, r.time_updated, ifnull((SELECT m.data FROM message m WHERE m.session_id = r.id ORDER BY m.time_created DESC LIMIT 1),'') FROM recent r ORDER BY r.time_updated DESC;"
        );
        let Some(legacy_output) = run_query(legacy_query) else {
            return;
        };
        if !legacy_output.status.success() {
            return;
        }
        output = legacy_output;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts = line.split(OPENCODE_SQL_SEP).collect::<Vec<_>>();
        if parts.len() < 5 || parts[4].is_empty() {
            continue;
        }
        let time_updated = parts[3].parse::<u64>().unwrap_or(now_ms);
        if let Some(snapshot) = opencode_snapshot_from_row(
            parts[0],
            (!parts[1].is_empty()).then_some(parts[1]),
            parts[2],
            time_updated,
            parts[4],
            parts.get(5).copied().filter(|value| !value.is_empty()),
            now_ms,
        ) {
            snapshots.push(snapshot);
        }
    }
}

fn scan_pi_sessions(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let sessions_dir = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .map(|dir| dir.join("sessions"))
        })
        .unwrap_or_else(|| home.join(".pi/agent/sessions"));

    for path in collect_jsonl_files(&sessions_dir) {
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Some(thread_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(snapshot) = pi_snapshot_from_jsonl(thread_id, &raw, mtime_ms, now_ms) {
            snapshots.push(snapshot);
        }
    }
}

fn scan_droid_sessions(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let projects_dir = std::env::var_os("FACTORY_PROJECTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".factory/projects"));

    for path in collect_jsonl_files(&projects_dir) {
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Some(thread_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(snapshot) = droid_snapshot_from_jsonl(thread_id, &raw, mtime_ms, now_ms) {
            snapshots.push(snapshot);
        }
    }
}

fn run_process_with_timeout(
    mut command: process::Command,
    timeout: Duration,
) -> Option<process::Output> {
    let mut child = command
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()
        .ok()?;
    let started = Instant::now();

    loop {
        if child.try_wait().ok()?.is_some() {
            return child.wait_with_output().ok();
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn collect_jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(collect_jsonl_files(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    files
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn encode_agent_project_dir(path: &str) -> String {
    path.chars()
        .map(|ch| match ch {
            '/' | '.' | '_' => '-',
            ch => ch,
        })
        .collect()
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn json_string_or_null(value: Option<&str>) -> String {
    value
        .map(|value| serde_json::to_string(value).expect("string must serialize"))
        .unwrap_or_else(|| "null".to_string())
}

fn activate_session_json(name: String, source_pane_id: Option<&str>) -> String {
    serde_json::to_string(&SidebarServerMessage::ActivateSession {
        name,
        source_pane_id: source_pane_id.map(str::to_string),
        target_pane_id: None,
        client_tty: None,
        target_thread_id: None,
    })
    .expect("activate-session must serialize")
}

fn activate_agent_pane_json(
    name: String,
    pane: &str,
    client_tty: Option<&str>,
    thread: Option<&str>,
) -> String {
    serde_json::to_string(&SidebarServerMessage::ActivateSession {
        name,
        source_pane_id: None,
        target_pane_id: Some(pane.to_string()),
        client_tty: client_tty.map(str::to_string),
        target_thread_id: thread.map(str::to_string),
    })
    .expect("activate-agent-pane must serialize")
}

fn parse_metadata_tone(value: &str) -> Option<MetadataTone> {
    match value {
        "neutral" => Some(MetadataTone::Neutral),
        "info" => Some(MetadataTone::Info),
        "success" => Some(MetadataTone::Success),
        "warn" => Some(MetadataTone::Warn),
        "error" => Some(MetadataTone::Error),
        _ => None,
    }
}

fn parse_agent_status(value: &str) -> Option<AgentStatus> {
    match value {
        "idle" => Some(AgentStatus::Idle),
        "running" => Some(AgentStatus::Running),
        "tool-running" => Some(AgentStatus::ToolRunning),
        "done" => Some(AgentStatus::Done),
        "error" => Some(AgentStatus::Error),
        "waiting" => Some(AgentStatus::Waiting),
        "interrupted" => Some(AgentStatus::Interrupted),
        "stale" => Some(AgentStatus::Stale),
        _ => None,
    }
}

fn parse_agent_panel_scope(value: &str) -> Option<AgentPanelScope> {
    match value {
        "current" => Some(AgentPanelScope::Current),
        "all" => Some(AgentPanelScope::All),
        _ => None,
    }
}

fn parse_process_row(line: &str) -> Option<(u32, u32)> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    Some((pid, ppid))
}

struct HttpContext {
    client_tty: Option<String>,
    session: String,
    window_id: String,
    pane_id: Option<String>,
    pane_active: Option<bool>,
}

fn parse_context(body: &str) -> Option<HttpContext> {
    let trimmed = trim_context_quotes(body);
    let pipe_parts = trimmed.split('|').collect::<Vec<_>>();
    if pipe_parts.len() == 5 && !pipe_parts[1].is_empty() && !pipe_parts[2].is_empty() {
        return Some(HttpContext {
            client_tty: (!pipe_parts[0].is_empty()).then(|| pipe_parts[0].to_string()),
            session: pipe_parts[1].to_string(),
            window_id: pipe_parts[2].to_string(),
            pane_id: Some(pipe_parts[3].to_string()),
            pane_active: Some(pipe_parts[4] == "1"),
        });
    }
    if pipe_parts.len() == 4 && !pipe_parts[1].is_empty() && !pipe_parts[2].is_empty() {
        return Some(HttpContext {
            client_tty: (!pipe_parts[0].is_empty()).then(|| pipe_parts[0].to_string()),
            session: pipe_parts[1].to_string(),
            window_id: pipe_parts[2].to_string(),
            pane_id: Some(pipe_parts[3].to_string()),
            pane_active: None,
        });
    }
    if pipe_parts.len() == 3 && !pipe_parts[1].is_empty() && !pipe_parts[2].is_empty() {
        return Some(HttpContext {
            client_tty: (!pipe_parts[0].is_empty()).then(|| pipe_parts[0].to_string()),
            session: pipe_parts[1].to_string(),
            window_id: pipe_parts[2].to_string(),
            pane_id: None,
            pane_active: None,
        });
    }

    let colon_idx = trimmed.find(':')?;
    if colon_idx < 1 {
        return None;
    }
    let session = &trimmed[..colon_idx];
    let window_id = &trimmed[colon_idx + 1..];
    (!session.is_empty() && !window_id.is_empty()).then(|| HttpContext {
        client_tty: None,
        session: session.to_string(),
        window_id: window_id.to_string(),
        pane_id: None,
        pane_active: None,
    })
}

fn parse_context_session(body: &str) -> Option<String> {
    parse_context(body).map(|context| context.session)
}

fn trim_context_quotes(value: &str) -> &str {
    trim_single_quotes(trim_double_quotes(value.trim()))
}

fn trim_double_quotes(value: &str) -> &str {
    value.trim_matches('"')
}

fn trim_single_quotes(value: &str) -> &str {
    value.trim_matches('\'')
}

#[derive(Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub pid_file: PathBuf,
    state_source: Option<Arc<dyn StateSource>>,
}

impl ServerConfig {
    pub fn new(host: impl Into<String>, port: u16, pid_file: impl Into<PathBuf>) -> Self {
        Self {
            host: host.into(),
            port,
            pid_file: pid_file.into(),
            state_source: None,
        }
    }

    pub fn with_state_source(mut self, source: impl StateSource) -> Self {
        self.state_source = Some(Arc::new(source));
        self
    }
}

#[derive(Debug)]
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: broadcast::Sender<()>,
    task: JoinHandle<Result<(), ServerError>>,
}

impl ServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn shutdown(self) -> Result<(), ServerError> {
        let _ = self.shutdown.send(());
        self.wait_shutdown().await
    }

    pub async fn wait_shutdown(self) -> Result<(), ServerError> {
        self.task.await.map_err(ServerError::from)?
    }
}

#[derive(Debug, Clone)]
pub struct ServerError {
    message: String,
}

impl ServerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<tokio_websockets::Error> for ServerError {
    fn from(value: tokio_websockets::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<tokio::task::JoinError> for ServerError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self::new(value.to_string())
    }
}

pub async fn start_server(config: ServerConfig) -> Result<ServerHandle, ServerError> {
    let bind_addr = (config.host.as_str(), config.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| ServerError::new("server bind address did not resolve"))?;
    let listener = TcpListener::bind(bind_addr).await?;
    let addr = listener.local_addr()?;
    fs::write(&config.pid_file, process::id().to_string())?;

    let (shutdown, shutdown_rx) = broadcast::channel(1);
    let (state_updates, _) = broadcast::channel(16);
    let shutdown_announcement = Arc::new(ShutdownAnnouncement::default());
    if let Some(source) = config.state_source.clone() {
        let _background_tasks = source
            .clone()
            .start_background_tasks(state_updates.clone(), shutdown.clone());
        source.setup_mux_hooks(&config.host, addr.port());
    }
    let task_shutdown = shutdown.clone();
    let state_source = config.state_source.clone();
    let cleanup_state_source = state_source.clone();
    let loop_shutdown_announcement = Arc::clone(&shutdown_announcement);
    let task = tokio::spawn(async move {
        let result = run_accept_loop(
            listener,
            task_shutdown,
            shutdown_rx,
            state_source,
            state_updates,
            loop_shutdown_announcement,
        )
        .await;
        if let Some(source) = cleanup_state_source.as_ref() {
            source.cleanup_mux_hooks();
        }
        let cleanup_result = fs::remove_file(&config.pid_file);
        match (result, cleanup_result) {
            (Err(err), _) => Err(err),
            (Ok(()), Err(err)) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    });

    Ok(ServerHandle {
        addr,
        shutdown,
        task,
    })
}

async fn run_accept_loop(
    listener: TcpListener,
    shutdown: broadcast::Sender<()>,
    mut shutdown_rx: broadcast::Receiver<()>,
    state_source: Option<Arc<dyn StateSource>>,
    state_updates: broadcast::Sender<String>,
    shutdown_announcement: Arc<ShutdownAnnouncement>,
) -> Result<(), ServerError> {
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                shutdown_announcement.announce_once(&state_source, &state_updates);
                tokio::time::sleep(Duration::from_millis(SERVER_SHUTDOWN_DRAIN_MS)).await;
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let connection_shutdown = shutdown.clone();
                let connection_state_source = state_source.clone();
                let connection_state_updates = state_updates.clone();
                let connection_shutdown_announcement = Arc::clone(&shutdown_announcement);
                tokio::spawn(async move {
                    let _ = handle_connection(
                        stream,
                        connection_shutdown,
                        connection_state_source,
                        connection_state_updates,
                        connection_shutdown_announcement,
                    )
                    .await;
                });
            }

        }
    }
}

fn announce_shutdown(
    state_source: &Option<Arc<dyn StateSource>>,
    state_updates: &broadcast::Sender<String>,
) {
    if let Some(payload) = state_source
        .as_ref()
        .and_then(|source| source.begin_shutdown())
    {
        let _ = state_updates.send(payload);
    }
    let _ = state_updates.send(QUIT_JSON.to_string());
}

fn request_shutdown(
    state_source: &Option<Arc<dyn StateSource>>,
    state_updates: &broadcast::Sender<String>,
    shutdown: &broadcast::Sender<()>,
    shutdown_announcement: &ShutdownAnnouncement,
) {
    shutdown_announcement.announce_once(state_source, state_updates);
    let _ = shutdown.send(());
}

async fn handle_connection(
    mut stream: TcpStream,
    shutdown: broadcast::Sender<()>,
    state_source: Option<Arc<dyn StateSource>>,
    state_updates: broadcast::Sender<String>,
    shutdown_announcement: Arc<ShutdownAnnouncement>,
) -> Result<(), ServerError> {
    let mut request = read_http_header(&mut stream).await?;
    let parsed = parse_http_request(&request)?;
    if parsed.path == "/api/agent-name" && parsed.content_length() > 4096 {
        write_agent_json(&mut stream, "413 Payload Too Large", &serde_json::json!({"error":{"code":"invalid_payload","message":"Agent naming request is too large"}})).await?;
        return Ok(());
    }
    read_remaining_http_body(&mut stream, &mut request, parsed.content_length()).await?;

    if matches!(parsed.path.as_str(), "/api/agents" | "/api/agent-name") {
        let result = match (parsed.method.as_str(), parsed.path.as_str()) {
            ("GET", "/api/agents") => state_source
                .as_ref()
                .ok_or_else(AgentNameError::unavailable)
                .and_then(|source| source.agent_list_json())
                .map(|body| (body, None)),
            ("POST", "/api/agent-name") => serde_json::from_slice::<Value>(http_body(&request))
                .map_err(|_| AgentNameError::invalid_payload())
                .and_then(|body| {
                    state_source
                        .as_ref()
                        .ok_or_else(AgentNameError::unavailable)
                        .and_then(|source| source.rename_agent_json(&body))
                }),
            _ => {
                write_agent_json(&mut stream, "405 Method Not Allowed", &serde_json::json!({"error":{"code":"method_not_allowed","message":"Use GET /api/agents or POST /api/agent-name"}})).await?;
                return Ok(());
            }
        };
        match result {
            Ok((body, snapshot)) => {
                if let Some(snapshot) = snapshot {
                    let _ = state_updates.send(snapshot);
                }
                write_agent_json(&mut stream, "200 OK", &body).await?;
            }
            Err(error) => write_agent_json(&mut stream, error.status, &error.body()).await?,
        }
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/refresh" {
        if let Some(state_source) = &state_source {
            if let Some(snapshot) = state_source.cached_snapshot_json() {
                let _ = state_updates.send(snapshot);
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/focus" {
        let body = String::from_utf8_lossy(http_body(&request)).into_owned();
        if let Some(source) = &state_source {
            if let Some(payload) = source.handle_http_text(&parsed.path, &body) {
                let _ = state_updates.send(payload);
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/switch-index" {
        let Some(index) = parsed
            .query_param("index")
            .and_then(|index| index.parse::<u32>().ok())
        else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 13\r\n\r\nmissing index")
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        };
        let body = String::from_utf8_lossy(http_body(&request));
        if let Some(state_source) = &state_source {
            let _ = state_source.handle_switch_index(index, &body);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && is_ok_hook_path(&parsed.path) {
        let body = String::from_utf8_lossy(http_body(&request)).into_owned();
        if let Some(source) = state_source.clone() {
            let path = parsed.path.clone();
            if let Some(payload) =
                tokio::task::spawn_blocking(move || source.handle_http_hook(&path, &body)).await?
            {
                let _ = state_updates.send(payload);
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/api/agent-event" {
        let Ok(body) = serde_json::from_slice::<Value>(http_body(&request)) else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\n\r\ninvalid json")
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        };
        match state_source
            .as_ref()
            .ok_or(AgentEventError::CouldNotResolveSession)
            .and_then(|state_source| state_source.handle_agent_event_json(&body))
        {
            Ok(payload) => {
                let _ = state_updates.send(payload);
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .await?;
            }
            Err(err) => {
                let (status, body) = err.status_and_body();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await?;
            }
        }
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/api/runtime/pi/upsert" {
        let Ok(body) = serde_json::from_slice::<Value>(http_body(&request)) else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\n\r\ninvalid json")
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        };
        if let Some(state_source) = &state_source
            && let Err(err) = state_source.handle_pi_runtime_upsert(&body)
        {
            let body = err.body();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/api/runtime/pi/delete" {
        let Ok(body) = serde_json::from_slice::<Value>(http_body(&request)) else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\n\r\ninvalid json")
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        };
        if let Some(state_source) = &state_source
            && let Err(err) = state_source.handle_pi_runtime_delete(&body)
        {
            let body = err.body();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST"
        && let Ok(body) = serde_json::from_slice::<Value>(http_body(&request))
        && is_metadata_path(&parsed.path)
        && !body.get("session").is_some_and(Value::is_string)
    {
        stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 15\r\n\r\nmissing session")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST"
        && let Ok(body) = serde_json::from_slice::<Value>(http_body(&request))
        && let Some(payload) = state_source
            .as_ref()
            .and_then(|state_source| state_source.handle_http_json(&parsed.path, &body))
    {
        let _ = state_updates.send(payload);
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/quit" {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        request_shutdown(
            &state_source,
            &state_updates,
            &shutdown,
            &shutdown_announcement,
        );
        return Ok(());
    }

    if parsed.is_websocket_upgrade() {
        let Some(key) = parsed.header("sec-websocket-key") else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await?;
            return Ok(());
        };
        let accept = websocket_accept(key);
        stream
            .write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;

        let mut websocket = ServerBuilder::new().serve(stream);
        debug_log("ws: client connected, sending hello + initial state");
        websocket.send(Message::text(HELLO_JSON)).await?;
        if let Some(state_source) = &state_source {
            websocket
                .send(Message::text(
                    initial_state_json(state_source.clone()).await?,
                ))
                .await?;
        }

        let mut connection_shutdown = shutdown.subscribe();
        let mut state_rx = state_updates.subscribe();
        let mut client_context = ClientConnectionContext::default();
        let mut pending_state: Option<String> = None;
        let mut state_flush =
            tokio::time::interval(Duration::from_millis(RENDERED_SIDEBAR_FRAME_MS));
        state_flush.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;

                _ = connection_shutdown.recv() => {
                    let _ = websocket.send(Message::text(QUIT_JSON)).await;
                    return Ok(());
                }
                message = websocket.next() => {
                    match message {
                        Some(Ok(message)) if message.is_close() => return Ok(()),
                        Some(Ok(message)) => {
                            if is_quit_command(&message) {
                                request_shutdown(
                                    &state_source,
                                    &state_updates,
                                    &shutdown,
                                    &shutdown_announcement,
                                );
                                return Ok(());
                            }
                            if is_command_type(&message, "refresh")
                                && let Some(state_source) = &state_source
                            {
                                let _ = state_updates.send(state_source.snapshot_json());
                            }
                            if let Some(command) = parse_command(&message) {
                                if let Some(reply) = state_source
                                    .as_ref()
                                    .and_then(|state_source| state_source.handle_sender_command_with_context(&command, &mut client_context))
                                {
                                    websocket.send(Message::text(reply)).await?;
                                    if command.get("type").and_then(Value::as_str) == Some("identify-pane")
                                        && let Some(focus) = state_source.as_ref()
                                            .and_then(|source| source.sender_agent_focus_json(&client_context)) {
                                        websocket.send(Message::text(focus)).await?;
                                    }
                                }
                                if let Some(name) = switch_session_target(&command) {
                                    let _ = state_updates.send(activate_session_json(
                                        name,
                                        client_context.pane_id.as_deref(),
                                    ));
                                    tokio::task::yield_now().await;
                                }
                                if let Some(payload) = state_source
                                    .as_ref()
                                    .and_then(|state_source| state_source.handle_client_command_with_context(&command, Some(&client_context)))
                                {
                                    if is_client_view_command(&command) {
                                        websocket.send(Message::text(payload)).await?;
                                    } else {
                                        let _ = state_updates.send(payload);
                                    }
                                }
                            }
                        }
                        Some(Err(err)) => return Err(err.into()),
                        None => return Ok(()),
                    }
                }
                _ = state_flush.tick(), if pending_state.is_some() => {
                    let state = pending_state.take().expect("pending state checked above");
                    debug_log(format!(
                        "ws: flushing latest broadcast state ({} bytes) to client",
                        state.len()
                    ));
                    websocket.send(Message::text(state)).await?;
                }
                state = state_rx.recv() => {
                    match state {
                        Ok(state) => {
                            if state == QUIT_JSON {
                                let _ = websocket.send(Message::text(QUIT_JSON)).await;
                                return Ok(());
                            }
                            if is_immediate_server_message(&state) {
                                websocket.send(Message::text(state)).await?;
                                continue;
                            }
                            pending_state = Some(state);
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            debug_log(format!("ws: state_rx lagged by {n} messages"));
                        }
                    }
                }
            }
        }
    }

    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 19\r\n\r\nherdmux server")
        .await?;
    Ok(())
}

fn app_from_state_json(state_json: &str) -> Option<SidebarApp> {
    let SidebarServerMessage::State(state) =
        serde_json::from_str::<SidebarServerMessage>(state_json).ok()?
    else {
        return None;
    };
    Some(SidebarApp::from_state(state))
}

async fn read_http_header(stream: &mut TcpStream) -> Result<Vec<u8>, ServerError> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];

    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(ServerError::new("client closed before sending request"));
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
        if request.len() > MAX_HTTP_HEADER_BYTES {
            return Err(ServerError::new("http request headers exceeded limit"));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct HttpRequest {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.as_str())
    }

    fn is_websocket_upgrade(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            && self
                .header("connection")
                .is_some_and(|value| contains_token_ignore_ascii_case(value, "upgrade"))
    }

    fn content_length(&self) -> usize {
        self.header("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
    }

    fn query_param(&self, name: &str) -> Option<&str> {
        self.query.as_deref()?.split('&').find_map(|part| {
            let (key, value) = part.split_once('=')?;
            (key == name).then_some(value)
        })
    }
}

fn parse_http_request(bytes: &[u8]) -> Result<HttpRequest, ServerError> {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| ServerError::new("http request missing header terminator"))?;
    let text = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| ServerError::new("http request headers were not utf-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| ServerError::new("http request missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| ServerError::new("http request missing method"))?
        .to_string();
    let target = request_parts
        .next()
        .ok_or_else(|| ServerError::new("http request missing target"))?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (target.to_string(), None),
    };

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();

    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
    })
}

fn contains_token_ignore_ascii_case(value: &str, needle: &str) -> bool {
    value
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case(needle))
}

fn is_metadata_path(path: &str) -> bool {
    matches!(
        path,
        "/set-status" | "/set-progress" | "/log" | "/notify" | "/clear-log"
    )
}

fn is_ok_hook_path(path: &str) -> bool {
    matches!(
        path,
        "/pane-exited" | "/pane-layout-changed" | "/client-resized" | "/ensure-sidebar" | "/toggle"
    )
}

async fn read_remaining_http_body(
    stream: &mut TcpStream,
    request: &mut Vec<u8>,
    content_length: usize,
) -> Result<(), ServerError> {
    let remaining = content_length.saturating_sub(http_body(request).len());
    if remaining == 0 {
        return Ok(());
    }

    let start_len = request.len();
    request.resize(start_len + remaining, 0);
    stream.read_exact(&mut request[start_len..]).await?;
    Ok(())
}

async fn write_agent_json(
    stream: &mut TcpStream,
    status: &str,
    body: &Value,
) -> Result<(), ServerError> {
    let body = serde_json::to_string(body).map_err(|error| ServerError::new(error.to_string()))?;
    stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn initial_state_json(source: Arc<dyn StateSource>) -> Result<String, ServerError> {
    if let Some(snapshot) = source.cached_snapshot_json() {
        return Ok(snapshot);
    }
    Ok(tokio::task::spawn_blocking(move || source.snapshot_json()).await?)
}

fn http_body(request: &[u8]) -> &[u8] {
    let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return &[];
    };
    &request[header_end + 4..]
}

fn websocket_accept(key: &str) -> String {
    let mut sha1 = Sha1::new();
    sha1.update(key.as_bytes());
    sha1.update(WEBSOCKET_GUID.as_bytes());
    STANDARD.encode(sha1.digest().bytes())
}

fn is_quit_command(message: &Message) -> bool {
    is_command_type(message, "quit")
}

fn is_command_type(message: &Message, command_type: &str) -> bool {
    parse_command(message)
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .as_deref()
        == Some(command_type)
}

fn is_client_view_command(command: &Value) -> bool {
    matches!(
        command.get("type").and_then(Value::as_str),
        Some("switch-session" | "switch-index")
    )
}

fn switch_session_target(command: &Value) -> Option<String> {
    (command.get("type").and_then(Value::as_str) == Some("switch-session"))
        .then(|| command.get("name")?.as_str().map(str::to_string))?
}

fn is_immediate_server_message(payload: &str) -> bool {
    payload.contains(r#""type":"activate-session""#)
}

fn clamp_detail_panel_height(height: u16) -> u16 {
    height.clamp(MIN_DETAIL_PANEL_HEIGHT, MAX_DETAIL_PANEL_HEIGHT)
}

fn parse_command(message: &Message) -> Option<Value> {
    serde_json::from_str::<Value>(message.as_text()?).ok()
}

#[cfg(test)]
mod agent_presence_tests {
    use super::*;
    use herdmux_runtime::mux::{AgentPane, MuxSessionInfo};

    #[test]
    fn passive_focus_targets_exact_live_pane_and_replays_for_identified_client() {
        let source = source_with_agent_panes(Vec::new());
        *source.confirmed_agent_panes.lock().unwrap() =
            HashMap::from([("work".into(), vec![live_pane("%7"), live_pane("%8")])]);
        let message = source
            .handle_http_text("/focus", "/dev/ttys1|work|@1|%7|1")
            .unwrap();
        let value: Value = serde_json::from_str(&message).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"type":"activate-session","name":"work",
            "targetPaneId":"%7","clientTty":"/dev/ttys1"})
        );
        assert!(
            source
                .handle_http_text("/focus", "/dev/ttys1|work|@1|%7|1")
                .is_none()
        );
        let context = ClientConnectionContext {
            client_tty: Some("/dev/ttys1".into()),
            session_name: Some("work".into()),
            ..Default::default()
        };
        assert_eq!(source.sender_agent_focus_json(&context), Some(message));
        assert!(
            source
                .sender_agent_focus_json(&ClientConnectionContext {
                    client_tty: Some("/dev/ttys2".into()),
                    ..context.clone()
                })
                .is_none()
        );
        assert!(
            source
                .handle_http_text("/focus", "/dev/ttys1|work|@1|%8|0")
                .is_none()
        );
        let next: Value = serde_json::from_str(
            &source
                .handle_http_text("/focus", "/dev/ttys1|work|@1|%8|1")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(next["targetPaneId"], "%8");
        assert!(
            source
                .handle_http_text("/focus", "/dev/ttys1|work|@1|%99|1")
                .is_none()
        );
        source.confirmed_agent_panes.lock().unwrap().clear();
        assert!(source.sender_agent_focus_json(&context).is_none());
    }

    #[test]
    fn explicit_focus_emits_target_without_new_inventory_even_when_already_focused() {
        let source = source_with_agent_panes(Vec::new());
        *source.confirmed_agent_panes.lock().unwrap() =
            HashMap::from([("work".into(), vec![live_pane("%7")])]);
        source
            .confirmed_inventory_ready
            .store(true, Ordering::Release);
        source
            .focused_pane_by_session
            .lock()
            .unwrap()
            .insert("work".into(), "%7".into());
        let context = ClientConnectionContext {
            client_tty: Some("/dev/ttys1".into()),
            ..Default::default()
        };
        let command = serde_json::json!({"type":"focus-agent-pane","session":"work",
            "agent":"claude-code","paneId":"%7"});
        let payload: Value = serde_json::from_str(
            &source
                .handle_client_command_with_context(&command, Some(&context))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(payload["targetPaneId"], "%7");
        assert_eq!(payload["clientTty"], "/dev/ttys1");
        assert!(
            source
                .handle_http_text("/focus", "/dev/ttys1|work|@1|%7|1")
                .is_none()
        );
    }

    #[test]
    fn legacy_activation_json_is_unchanged_and_decodes_without_new_fields() {
        let message = activate_session_json("work".into(), Some("%9"));
        assert_eq!(
            message,
            r#"{"type":"activate-session","name":"work","sourcePaneId":"%9"}"#
        );
        let decoded: SidebarServerMessage = serde_json::from_str(&message).unwrap();
        assert!(matches!(
            decoded,
            SidebarServerMessage::ActivateSession {
                target_pane_id: None,
                client_tty: None,
                ..
            }
        ));
    }

    #[test]
    fn supplied_pane_focus_requires_cached_family_thread_and_session() {
        let source = source_with_agent_panes(Vec::new());
        let pane = PanePresenceInput {
            thread_id: Some("T-1".into()),
            ..live_pane("%7")
        };
        *source.confirmed_agent_panes.lock().unwrap() =
            HashMap::from([("work".into(), vec![pane])]);
        source
            .confirmed_inventory_ready
            .store(true, Ordering::Release);
        assert!(
            source
                .resolve_agent_pane("work", "claude-code", Some("T-1"), None, Some("%7"))
                .is_some()
        );
        for (session, family, thread, pane) in [
            ("work", "codex", "T-1", "%7"),
            ("work", "claude-code", "T-other", "%7"),
            ("other", "claude-code", "T-1", "%7"),
            ("work", "claude-code", "T-1", "%9"),
        ] {
            assert!(
                source
                    .resolve_agent_pane(session, family, Some(thread), None, Some(pane))
                    .is_none()
            );
        }
        source.confirmed_agent_panes.lock().unwrap().clear();
        assert!(
            source
                .resolve_agent_pane("work", "claude-code", Some("T-1"), None, Some("%7"))
                .is_none()
        );
    }

    #[test]
    fn explicit_focus_deduplicates_bursts_but_not_historical_focus() {
        let source = source_with_agent_panes(Vec::new());
        assert!(source.should_focus_agent_pane("work", "%7"));
        assert!(!source.should_focus_agent_pane("work", "%7"));
        source
            .focused_pane_by_session
            .lock()
            .unwrap()
            .insert("work".into(), "%8".into());
        assert!(!source.should_focus_agent_pane("work", "%7"));
        assert!(source.should_focus_agent_pane("work", "%9"));
        source
            .focused_pane_by_session
            .lock()
            .unwrap()
            .insert("work".into(), "%9".into());
        *source.last_explicit_agent_focus.lock().unwrap() = Some((
            "work".into(),
            "%9".into(),
            Instant::now() - std::time::Duration::from_millis(AGENT_FOCUS_COOLDOWN_MS + 1),
        ));
        assert!(source.should_focus_agent_pane("work", "%9"));
    }

    #[test]
    fn live_pane_title_overrides_transcript_label_in_outgoing_rows() {
        let pane = PanePresenceInput {
            thread_name: Some("api-reviewer".into()),
            ..live_pane("%7")
        };
        let tracked = AgentEvent {
            thread_name: Some("Long transcript prompt".into()),
            ..agents_with_live_panes("liquid", Vec::new(), &[live_pane("%7")]).remove(0)
        };
        assert_eq!(
            agents_with_live_panes("liquid", vec![tracked], &[pane])[0]
                .thread_name
                .as_deref(),
            Some("api-reviewer")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_websocket_state_uses_cache_without_native_snapshot() {
        struct CachedSource;
        impl StateSource for CachedSource {
            fn snapshot_json(&self) -> String {
                panic!("initial connection must not trigger another native inventory")
            }
            fn cached_snapshot_json(&self) -> Option<String> {
                Some("cached state".into())
            }
        }
        assert_eq!(
            initial_state_json(Arc::new(CachedSource)).await.unwrap(),
            "cached state"
        );
    }

    async fn agent_api_request(
        source: Arc<ReadOnlyMuxStateSource>,
        method: &str,
        path: &str,
        body: &str,
    ) -> (
        String,
        broadcast::Receiver<String>,
        broadcast::Receiver<String>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, _) = broadcast::channel(4);
        let (updates, first) = broadcast::channel(4);
        let second = updates.subscribe();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                shutdown,
                Some(source),
                updates,
                Arc::new(ShutdownAnnouncement::default()),
            )
            .await
            .unwrap();
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap();
        (String::from_utf8(response).unwrap(), first, second)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn naming_endpoints_list_rename_clear_and_reject_invalid_json() {
        let source = Arc::new(source_with_agent_panes(Vec::new()));
        *source.confirmed_agent_panes.lock().unwrap() =
            HashMap::from([("liquid".into(), vec![live_pane("%7")])]);
        source
            .confirmed_inventory_ready
            .store(true, Ordering::Release);
        let (list, _, _) = agent_api_request(source.clone(), "GET", "/api/agents", "").await;
        assert!(list.starts_with("HTTP/1.1 200 OK"));
        assert!(list.contains("Unnamed"));
        let (renamed, _, _) = agent_api_request(
            source.clone(),
            "POST",
            "/api/agent-name",
            r#"{"paneId":"%7","name":"api-reviewer"}"#,
        )
        .await;
        assert!(renamed.starts_with("HTTP/1.1 200 OK") && renamed.contains("api-reviewer"));
        let (invalid, _, _) =
            agent_api_request(source.clone(), "POST", "/api/agent-name", "{").await;
        assert!(
            invalid.starts_with("HTTP/1.1 400 Bad Request") && invalid.contains("invalid_payload")
        );
        let (cleared, _, _) = agent_api_request(
            source,
            "POST",
            "/api/agent-name",
            r#"{"paneId":"%7","name":null}"#,
        )
        .await;
        assert!(cleared.starts_with("HTTP/1.1 200 OK") && cleared.contains("Unnamed"));
    }

    #[test]
    fn hook_reported_needs_input_marks_live_agent_waiting() {
        let pane = PanePresenceInput {
            needs_input: true,
            ..live_pane("%7")
        };
        let stale = AgentEvent {
            status: AgentStatus::Stale,
            ..agents_with_live_panes("liquid", Vec::new(), &[live_pane("%7")]).remove(0)
        };
        let agents = agents_with_live_panes("liquid", vec![stale], &[pane]);
        assert_eq!(agents[0].status, AgentStatus::Waiting);
        let agents = agents_with_live_panes("liquid", Vec::new(), &[live_pane("%7")]);
        assert_ne!(agents[0].status, AgentStatus::Waiting);
    }

    #[test]
    fn hook_reported_turn_in_progress_beats_a_quiet_transcript() {
        let pane = PanePresenceInput {
            working: true,
            ..live_pane("%7")
        };
        let done = AgentEvent {
            status: AgentStatus::Done,
            ..agents_with_live_panes("liquid", Vec::new(), &[live_pane("%7")]).remove(0)
        };
        let agents = agents_with_live_panes("liquid", vec![done.clone()], &[pane.clone()]);
        assert_eq!(agents[0].status, AgentStatus::Running);
        // More specific transcript states are kept.
        let tools = AgentEvent {
            status: AgentStatus::ToolRunning,
            ..done
        };
        let agents = agents_with_live_panes("liquid", vec![tools], &[pane]);
        assert_eq!(agents[0].status, AgentStatus::ToolRunning);
    }

    fn live_pane(id: &str) -> PanePresenceInput {
        PanePresenceInput {
            agent: "claude-code".into(),
            pane_id: id.into(),
            active: false,
            thread_id: None,
            thread_name: None,
            needs_input: false,
            working: false,
        }
    }

    #[test]
    fn live_agent_panes_have_individual_rows_without_recent_transcripts() {
        let panes = (1..=5)
            .map(|index| live_pane(&format!("%{index}")))
            .collect::<Vec<_>>();
        let agents = agents_with_live_panes("liquid", Vec::new(), &panes);
        assert_eq!(agents.len(), 5);
        assert!(agents.iter().all(|agent| agent.status == AgentStatus::Idle
            && agent.liveness == Some(AgentLiveness::Alive)));
        assert_eq!(
            agents
                .iter()
                .filter_map(|agent| agent.pane_id.as_deref())
                .collect::<HashSet<_>>()
                .len(),
            5
        );
    }

    #[test]
    fn newest_logical_owner_replaces_one_pane_fallback_without_duplicates() {
        let fallback = agents_with_live_panes("liquid", Vec::new(), &[live_pane("%7")]).remove(0);
        let first = AgentEvent {
            ts: 1,
            thread_id: Some("old-thread".into()),
            ..fallback.clone()
        };
        let newest = AgentEvent {
            ts: 2,
            thread_id: Some("new-thread".into()),
            status: AgentStatus::Done,
            ..fallback
        };
        assert_eq!(
            agents_with_live_panes(
                "liquid",
                vec![first, newest.clone()],
                &[live_pane("%7"), live_pane("%7")]
            ),
            vec![newest]
        );
    }

    #[test]
    fn vanished_live_pane_has_no_display_fallback() {
        let tracked = agents_with_live_panes("liquid", Vec::new(), &[live_pane("%7")]);
        assert!(agents_with_live_panes("liquid", tracked, &[]).is_empty());
    }

    #[test]
    fn default_state_source_honors_persisted_live_view_settings() {
        let home = std::env::temp_dir().join(format!(
            "herdmux-live-view-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config_dir = home.join(".config/herdmux");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.json"),
            r#"{"sessionFilter":"active","agentPanelScope":"all","detailPanelHeight":12}"#,
        )
        .unwrap();
        let source = default_state_source_from_env(|key| match key {
            "HOME" => Some(home.to_string_lossy().into_owned()),
            "TMUX" => Some("mock socket".into()),
            _ => None,
        })
        .unwrap();
        fs::remove_dir_all(home).unwrap();
        assert_eq!(
            *source.session_filter.lock().unwrap(),
            Some(SessionFilterMode::Active)
        );
        assert_eq!(
            *source.agent_panel_scope.lock().unwrap(),
            AgentPanelScope::All
        );
        assert_eq!(*source.detail_panel_height.lock().unwrap(), 12);
    }

    struct AgentProvider {
        panes: Mutex<Vec<AgentPane>>,
    }

    impl MuxProvider for AgentProvider {
        fn name(&self) -> &str {
            "fixture"
        }
        fn list_sessions(&self) -> Vec<MuxSessionInfo> {
            vec![MuxSessionInfo {
                name: "work".into(),
                dir: "/fixture".into(),
                created_at: 0,
                windows: 1,
            }]
        }
        fn switch_session(&self, _: &str, _: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            Some("work".into())
        }
        fn get_session_dir(&self, _: &str) -> String {
            "/fixture".into()
        }
        fn get_pane_count(&self, _: &str) -> u32 {
            1
        }
        fn get_client_tty(&self) -> String {
            String::new()
        }
        fn create_session(&self, _: Option<&str>, _: Option<&str>) {}
        fn kill_session(&self, _: &str) {}
        fn setup_hooks(&self, _: &str, _: u16) {}
        fn cleanup_hooks(&self) {}
        fn list_agent_panes(&self, _: &str) -> Vec<AgentPane> {
            self.panes.lock().unwrap().clone()
        }
        fn resolve_agent_pane_id(
            &self,
            _: &str,
            agent: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Option<String> {
            self.panes
                .lock()
                .unwrap()
                .iter()
                .find(|pane| pane.agent == agent)
                .map(|pane| pane.pane_id.clone())
        }
    }

    fn source_with_agent_panes(panes: Vec<AgentPane>) -> ReadOnlyMuxStateSource {
        ReadOnlyMuxStateSource::new(vec![Arc::new(AgentProvider {
            panes: Mutex::new(panes),
        })])
    }

    fn watcher_snapshot() -> AgentWatcherSnapshot {
        AgentWatcherSnapshot {
            agent: "codex",
            thread_id: Some("T-1".into()),
            thread_name: Some("Recent conversation".into()),
            last_user_prompt: None,
            project_dir: Some("/fixture".into()),
            status: AgentStatus::Done,
            ts: 1,
        }
    }

    #[test]
    fn recent_watcher_snapshot_without_live_agent_pane_does_not_create_ghost() {
        let source = source_with_agent_panes(Vec::new());
        assert!(!source.apply_agent_watcher_snapshot(watcher_snapshot()));
        assert!(
            source
                .agent_tracker
                .lock()
                .unwrap()
                .get_agents("work")
                .is_empty()
        );
    }

    #[test]
    fn completed_watcher_snapshot_with_live_agent_pane_remains_visible() {
        let source = source_with_agent_panes(vec![AgentPane {
            agent: "codex".into(),
            pane_id: "%7".into(),
            active: false,
            thread_id: Some("T-1".into()),
            thread_name: None,
            needs_input: false,
            working: false,
        }]);
        assert!(source.apply_agent_watcher_snapshot(watcher_snapshot()));
        let state = source
            .agent_tracker
            .lock()
            .unwrap()
            .get_state("work")
            .unwrap();
        assert_eq!(state.status, AgentStatus::Done);
        assert_eq!(state.pane_id.as_deref(), Some("%7"));
        assert_eq!(state.liveness, Some(AgentLiveness::Alive));
    }

    #[test]
    fn external_codex_event_without_live_pane_retains_unknown_liveness() {
        let source = source_with_agent_panes(Vec::new());
        source
            .apply_agent_event(
                &serde_json::json!({"agent":"codex", "status":"running", "tmuxSession":"work"}),
            )
            .unwrap();
        source.sync_agent_pane_presence();
        let state = source
            .agent_tracker
            .lock()
            .unwrap()
            .get_state("work")
            .unwrap();
        assert_eq!(state.liveness, None);
        assert_eq!(state.pane_id, None);
    }

    #[test]
    fn same_watcher_thread_resume_restores_row_without_reapplying_snapshot() {
        let pane = AgentPane {
            agent: "codex".into(),
            pane_id: "%7".into(),
            active: false,
            thread_id: Some("T-1".into()),
            thread_name: None,
            needs_input: false,
            working: false,
        };
        let provider = Arc::new(AgentProvider {
            panes: Mutex::new(vec![pane.clone()]),
        });
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        assert!(source.apply_agent_watcher_snapshot(watcher_snapshot()));

        provider.panes.lock().unwrap().clear();
        source.sync_agent_pane_presence();
        assert!(
            source
                .agent_tracker
                .lock()
                .unwrap()
                .get_state("work")
                .is_none()
        );

        provider.panes.lock().unwrap().push(pane);
        source.sync_agent_pane_presence();
        let resumed = source
            .agent_tracker
            .lock()
            .unwrap()
            .get_state("work")
            .unwrap();
        assert_eq!(resumed.thread_id.as_deref(), Some("T-1"));
        assert_eq!(resumed.liveness, Some(AgentLiveness::Alive));
    }

    #[test]
    fn dismiss_agent_command_removes_external_event() {
        let source = source_with_agent_panes(Vec::new());
        source.apply_agent_event(&serde_json::json!({"agent":"codex", "status":"running", "tmuxSession":"work", "threadId":"external"})).unwrap();
        source.handle_client_command(&serde_json::json!({"type":"dismiss-agent", "session":"work", "agent":"codex", "threadId":"external"}));
        assert!(
            source
                .agent_tracker
                .lock()
                .unwrap()
                .get_agents("work")
                .is_empty()
        );
    }
}
