use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::agent_watchers::parse_codex_session_index;

const TITLE_BYTES: u64 = 256 * 1024;
type IdentityMap = HashMap<(String, String), AgentIdentity>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentIdentity {
    pub thread_id: String,
    pub title: Option<String>,
    /// The `ai hook` binding reports the agent is blocked on the user.
    pub needs_input: bool,
}

#[derive(Deserialize)]
struct Binding {
    id: String,
    pane: String,
    tool: String,
    status: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    transcript: Option<String>,
    #[serde(default)]
    updated_at: String,
}

impl Binding {
    fn agent(&self) -> &'static str {
        if self.tool == "claude" {
            "claude-code"
        } else {
            "codex"
        }
    }
}

fn parse_binding(raw: &str) -> Option<Binding> {
    let record: Binding = serde_json::from_str(raw).ok()?;
    let pane = record.pane.strip_prefix('%')?;
    if pane.is_empty()
        || !pane.bytes().all(|byte| byte.is_ascii_digit())
        || record.id.is_empty()
        || !record
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || !matches!(record.tool.as_str(), "claude" | "codex")
        || !matches!(
            record.status.as_str(),
            "working" | "needs_input" | "done" | "idle" | "ended"
        )
    {
        return None;
    }
    Some(record)
}

fn latest_bindings(records: Vec<Binding>) -> Vec<Binding> {
    let mut latest: HashMap<(String, String), Binding> = HashMap::new();
    for record in records {
        let key = (record.agent().to_string(), record.pane.clone());
        if latest
            .get(&key)
            .is_none_or(|old| (&old.updated_at, &old.id) < (&record.updated_at, &record.id))
        {
            latest.insert(key, record);
        }
    }
    latest
        .into_values()
        .filter(|record| record.status != "ended")
        .collect()
}

#[derive(Clone, Default)]
struct NativeTitles {
    custom: Option<String>,
    automatic: Option<String>,
}

#[derive(Deserialize)]
struct TitleRecord {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "customTitle")]
    custom: Option<String>,
    #[serde(default, rename = "aiTitle")]
    automatic: Option<String>,
}

fn title_updates(raw: &str) -> NativeTitles {
    let mut titles = NativeTitles::default();
    for line in raw
        .lines()
        .filter(|line| line.contains("\"custom-title\"") || line.contains("\"ai-title\""))
    {
        let Ok(record) = serde_json::from_str::<TitleRecord>(line) else {
            continue;
        };
        match record.kind.as_str() {
            "custom-title" => titles.custom = record.custom.and_then(|title| useful_title(&title)),
            "ai-title" => {
                titles.automatic = record.automatic.and_then(|title| useful_title(&title))
            }
            _ => {}
        }
    }
    titles
}

fn useful_title(title: &str) -> Option<String> {
    let title = title.trim();
    (!title.is_empty() && !title.chars().any(char::is_control)).then(|| title.to_string())
}

pub(crate) fn useful_pane_title(title: &str, agent: &str) -> Option<String> {
    let meaningful = title
        .trim_start_matches(|character: char| !character.is_alphanumeric())
        .to_lowercase();
    if meaningful == agent
        || matches!(
            meaningful.as_str(),
            "claude" | "claude code" | "codex" | "node" | "zsh" | "bash" | "opensessions-sidebar"
        )
    {
        return None;
    }
    useful_title(title)
}

#[derive(Clone, Default)]
struct TranscriptCursor {
    offset: u64,
    titles: NativeTitles,
}

fn read_slice(path: &Path, offset: u64, limit: u64) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut preceding = [b'\n'];
    if offset > 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        file.read_exact(&mut preceding)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut raw = Vec::new();
    file.take(limit).read_to_end(&mut raw)?;
    let raw = String::from_utf8_lossy(&raw);
    Ok(if offset > 0 && preceding[0] != b'\n' {
        raw.split_once('\n')
            .map_or("", |(_, complete)| complete)
            .to_string()
    } else {
        raw.into_owned()
    })
}

fn transcript_title(
    path: &Path,
    cursors: &mut HashMap<PathBuf, TranscriptCursor>,
) -> io::Result<Option<String>> {
    let size = fs::metadata(path)?.len();
    let previous = cursors.get(path);
    let reset = previous.is_none_or(|cursor| size < cursor.offset);
    let mut cursor = if reset {
        TranscriptCursor::default()
    } else {
        previous.cloned().unwrap_or_default()
    };
    if reset && size > TITLE_BYTES {
        cursor.titles = title_updates(&read_slice(path, 0, TITLE_BYTES)?);
    }
    if reset || size > cursor.offset {
        let offset = if reset {
            size.saturating_sub(TITLE_BYTES)
        } else {
            cursor.offset.max(size.saturating_sub(TITLE_BYTES))
        };
        let updates = title_updates(&read_slice(path, offset, TITLE_BYTES)?);
        cursor.titles.custom = updates.custom.or(cursor.titles.custom);
        cursor.titles.automatic = updates.automatic.or(cursor.titles.automatic);
        cursor.offset = size;
    }
    let title = cursor
        .titles
        .custom
        .clone()
        .or_else(|| cursor.titles.automatic.clone());
    cursors.insert(path.to_path_buf(), cursor);
    Ok(title)
}

fn identity_for(
    record: &Binding,
    index: &HashMap<String, String>,
    native: Option<String>,
) -> AgentIdentity {
    let title = if record.tool == "codex" {
        index.get(&record.id).and_then(|title| useful_title(title))
    } else {
        native
    };
    AgentIdentity {
        thread_id: record.id.clone(),
        title: title.or_else(|| record.title.as_deref().and_then(useful_title)),
        needs_input: record.status == "needs_input",
    }
}

fn scan(home: &Path, cursors: &mut HashMap<PathBuf, TranscriptCursor>) -> io::Result<IdentityMap> {
    let entries = match fs::read_dir(home.join(".ai/sessions")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error),
    };
    let mut records = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !name.ends_with(".json") || !(name.starts_with("claude-") || name.starts_with("codex-"))
        {
            continue;
        }
        let raw = match read_slice(&path, 0, 64 * 1024) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if let Some(record) = parse_binding(&raw)
            .filter(|record| name == format!("{}-{}.json", record.tool, record.id))
        {
            records.push(record);
        }
    }
    let index = match read_slice(&home.join(".codex/session_index.jsonl"), 0, 1024 * 1024) {
        Ok(raw) => parse_codex_session_index(&raw).into_iter().collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => HashMap::new(),
        Err(error) => return Err(error),
    };
    let mut identities = HashMap::new();
    for record in latest_bindings(records) {
        let native = if record.tool == "claude" {
            record
                .transcript
                .as_deref()
                .map(Path::new)
                .filter(|path| {
                    path.file_name().and_then(|name| name.to_str())
                        == Some(&format!("{}.jsonl", record.id))
                })
                .map(|path| transcript_title(path, cursors))
                .transpose()?
                .flatten()
        } else {
            None
        };
        identities.insert(
            (record.agent().to_string(), record.pane.clone()),
            identity_for(&record, &index, native),
        );
    }
    Ok(identities)
}

#[derive(Default)]
struct CachedIdentities {
    attempted_at: Option<Instant>,
    identities: Arc<IdentityMap>,
    cursors: HashMap<PathBuf, TranscriptCursor>,
}

pub(crate) struct AgentIdentityCache {
    home: PathBuf,
    ttl: Duration,
    cached: Mutex<CachedIdentities>,
}

impl Default for AgentIdentityCache {
    fn default() -> Self {
        Self::new(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            Duration::from_secs(5),
        )
    }
}

impl AgentIdentityCache {
    pub(crate) fn new(home: PathBuf, ttl: Duration) -> Self {
        Self {
            home,
            ttl,
            cached: Mutex::new(CachedIdentities::default()),
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<IdentityMap> {
        if !self.home.is_absolute() {
            return Arc::new(HashMap::new());
        }
        let Ok(mut cached) = self.cached.lock() else {
            return Arc::new(HashMap::new());
        };
        if cached
            .attempted_at
            .is_none_or(|at| at.elapsed() >= self.ttl)
        {
            cached.attempted_at = Some(Instant::now());
            let mut cursors = cached.cursors.clone();
            // A failed observation cannot invalidate the last successfully read native names.
            if let Ok(identities) = scan(&self.home, &mut cursors) {
                cached.identities = Arc::new(identities);
                cached.cursors = cursors;
            }
        }
        Arc::clone(&cached.identities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_parser_uses_only_identity_and_title_fields() {
        let record = parse_binding(r#"{"id":"thread-1","pane":"%9","tool":"claude","status":"working","title":"API reviewer","updated_at":"2026-10-13T12:00:00+00:00","last_prompt":"must never become title"}"#).unwrap();
        assert_eq!(record.id, "thread-1");
        assert_eq!(record.title.as_deref(), Some("API reviewer"));
    }

    #[test]
    fn claude_reads_only_native_title_records_not_messages() {
        let raw = r#"{"type":"user","message":{"content":"secret prompt"}}
{"type":"ai-title","aiTitle":"Native initial title"}
{"type":"custom-title","customTitle":"Actual renamed session"}
{"type":"ai-title","aiTitle":"Later automatic title"}"#;
        assert_eq!(
            title_updates(raw).custom.as_deref(),
            Some("Actual renamed session")
        );
        assert_eq!(
            title_updates(raw).automatic.as_deref(),
            Some("Later automatic title")
        );
    }

    #[test]
    fn no_message_or_generic_pane_title_becomes_an_identity_name() {
        assert_eq!(
            title_updates(r#"{"type":"user","message":{"content":"generated fallback"}}"#).custom,
            None
        );
        assert_eq!(useful_pane_title("✳ Claude Code", "claude-code"), None);
        assert_eq!(useful_pane_title("Codex", "codex"), None);
        assert_eq!(
            useful_pane_title("API reviewer", "claude-code").as_deref(),
            Some("API reviewer")
        );
    }

    #[test]
    fn newest_binding_controls_pane_identity_and_ended_bindings_do_not_revive_old_threads() {
        let old = parse_binding(r#"{"id":"old","pane":"%9","tool":"claude","status":"idle","updated_at":"2026-10-13T10:00:00+00:00"}"#).unwrap();
        let ended = parse_binding(r#"{"id":"new","pane":"%9","tool":"claude","status":"ended","updated_at":"2026-10-13T11:00:00+00:00"}"#).unwrap();
        assert!(latest_bindings(vec![old, ended]).is_empty());
    }

    #[test]
    fn codex_index_title_wins_over_binding_title() {
        let record = parse_binding(r#"{"id":"codex-id","pane":"%11","tool":"codex","status":"working","title":"older clipped prompt","updated_at":"2026-10-13T12:00:00+00:00"}"#).unwrap();
        let index = HashMap::from([("codex-id".to_string(), "Review A2 assignment".to_string())]);
        assert_eq!(
            identity_for(&record, &index, None).title.as_deref(),
            Some("Review A2 assignment")
        );
    }
}
