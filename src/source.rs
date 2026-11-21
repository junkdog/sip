//! Reads Claude Code session transcripts (`~/.claude/projects/**/*.jsonl`).

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use jiff::Timestamp;
use rayon::prelude::*;
use serde::Deserialize;
use serde::de::IgnoredAny;
use walkdir::WalkDir;

use crate::model::{Category, Tokens};
use crate::pricing::{self, Usage as Billed};

/// One deduplicated assistant API call.
#[derive(Debug, Clone)]
pub struct Turn {
    pub ts: Timestamp,
    pub session: String,
    /// Set for subagent (sidechain) transcripts.
    pub agent: Option<String>,
    pub cwd: String,
    pub model: String,
    pub tokens: Tokens,
    /// Estimated USD; `None` when the model has no known price.
    pub cost: Option<f64>,
}

#[derive(Debug, Default)]
pub struct Transcripts {
    pub turns: Vec<Turn>,
    /// Session id -> models that ran with the 1M context window (`[1m]` suffix stripped).
    pub one_million: HashMap<String, HashSet<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(rename = "type")]
    kind: String,
    session_id: Option<String>,
    agent_id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<Timestamp>,
    request_id: Option<String>,
    message: Option<Message>,
    model_usage: Option<HashMap<String, IgnoredAny>>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    cache_creation: Option<CacheCreation>,
    output_tokens_details: Option<OutputDetails>,
}

/// Absent on the partial usage of early streamed blocks and on most subagent calls.
#[derive(Deserialize)]
struct OutputDetails {
    #[serde(default)]
    thinking_tokens: u64,
}

#[derive(Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

pub fn default_dir() -> PathBuf {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".claude")
        });
    config.join("projects")
}

pub fn load(dir: &Path) -> Result<Transcripts> {
    anyhow::ensure!(
        dir.is_dir(),
        "transcript directory not found: {}",
        dir.display()
    );

    let files: Vec<PathBuf> = WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| e.into_path())
        .collect();

    let parsed: Vec<Parsed> = files
        .par_iter()
        .filter_map(|f| File::open(f).ok())
        .map(|f| parse(BufReader::new(f)))
        .collect();
    Ok(merge(parsed))
}

/// Message id + request id: the same API call, in whichever transcript it was logged.
type CallId = (String, String);

/// One transcript's calls, before they are deduplicated against the other transcripts.
#[derive(Default)]
struct Parsed {
    calls: Vec<(Option<CallId>, Turn)>,
    one_million: HashMap<String, HashSet<String>>,
}

/// Streamed responses are logged once per content block, and a forked subagent opens its own
/// transcript with a copy of the parent's last call; all copies share the call's id. Keeps the
/// most complete copy of each call, and the main thread's over a subagent's on a tie.
fn merge(parsed: Vec<Parsed>) -> Transcripts {
    let mut out = Transcripts::default();
    let mut seen: HashMap<CallId, usize> = HashMap::new();
    for file in parsed {
        for (id, turn) in file.calls {
            let Some(id) = id else {
                out.turns.push(turn);
                continue;
            };
            match seen.get(&id) {
                Some(&i) if supersedes(&turn, &out.turns[i]) => out.turns[i] = turn,
                Some(_) => {}
                None => {
                    seen.insert(id, out.turns.len());
                    out.turns.push(turn);
                }
            }
        }
        for (session, models) in file.one_million {
            out.one_million.entry(session).or_default().extend(models);
        }
    }
    out.turns.sort_by_key(|t| t.ts);
    out
}

fn supersedes(copy: &Turn, kept: &Turn) -> bool {
    let output = |t: &Turn| t.tokens.get(Category::Output);
    match output(copy).cmp(&output(kept)) {
        Ordering::Greater => true,
        Ordering::Equal => copy.agent.is_none() && kept.agent.is_some(),
        Ordering::Less => false,
    }
}

fn parse(mut reader: impl BufRead) -> Parsed {
    let mut out = Parsed::default();
    let mut buf = Vec::new();
    // read bytes, so that a line which isn't valid UTF-8 costs that line rather than the rest of the file
    while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
        parse_line(&String::from_utf8_lossy(&buf), &mut out);
        buf.clear();
    }
    out
}

fn parse_line(line: &str, out: &mut Parsed) {
    let is_assistant = line.contains(r#""usage""#) && line.contains(r#""type":"assistant""#);
    let is_cost = line.contains(r#""type":"cost-state""#);
    if !is_assistant && !is_cost {
        return;
    }
    let Ok(entry) = serde_json::from_str::<Entry>(line) else {
        return;
    };

    match entry.kind.as_str() {
        "cost-state" => {
            let (Some(session), Some(models)) = (entry.session_id, entry.model_usage) else {
                return;
            };
            let big: HashSet<String> = models
                .into_keys()
                .filter_map(|m| m.strip_suffix("[1m]").map(str::to_string))
                .collect();
            if !big.is_empty() {
                out.one_million.entry(session).or_default().extend(big);
            }
        }
        "assistant" => {
            let Some(turn) = to_turn(&entry) else { return };
            let id = entry
                .message
                .as_ref()
                .and_then(|m| m.id.clone())
                .zip(entry.request_id.clone());
            out.calls.push((id, turn));
        }
        _ => {}
    }
}

/// Where a full load records every model it saw, so `--model` completion needn't reread
/// gigabytes of transcripts on each tab press: `$XDG_CACHE_HOME/sip/models` or `~/.cache/sip/models`.
fn models_cache() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("sip").join("models"))
}

/// Models recorded by earlier runs, one per line; `None` before the first run.
pub fn cached_models() -> Option<BTreeSet<String>> {
    let text = fs::read_to_string(models_cache()?).ok()?;
    Some(text.lines().map(str::to_string).collect())
}

/// Adds the models in `data` to the cache, keeping those recorded before (e.g. from another
/// `--dir`). Best effort: a cache that can't be written only makes completion slower.
pub fn remember_models(data: &Transcripts) {
    let Some(path) = models_cache() else { return };
    let seen: BTreeSet<&str> = data.turns.iter().map(|t| t.model.as_str()).collect();
    let mut models = cached_models().unwrap_or_default();
    let before = models.len();
    models.extend(seen.into_iter().map(str::to_string));
    if models.len() == before && path.exists() {
        return;
    }
    let text: String = models.iter().map(|m| format!("{m}\n")).collect();
    // written aside and renamed into place, so completion never reads half a file
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = path.parent().is_some_and(|d| fs::create_dir_all(d).is_ok())
        && fs::write(&tmp, text).is_ok()
        && fs::rename(&tmp, &path).is_ok();
    if !written {
        let _ = fs::remove_file(&tmp);
    }
}

fn to_turn(entry: &Entry) -> Option<Turn> {
    let message = entry.message.as_ref()?;
    let usage = message.usage.as_ref()?;
    let model = message.model.clone()?;
    if model.starts_with('<') {
        return None; // "<synthetic>" placeholder messages
    }
    // without the TTL breakdown, cache writes are assumed to be the default 5 minutes
    let (cache_write_5m, cache_write_1h) = match &usage.cache_creation {
        Some(c) => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
        None => (usage.cache_creation_input_tokens, 0),
    };
    let cost = pricing::cost(
        &model,
        &Billed {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_write_5m,
            cache_write_1h,
            cache_read: usage.cache_read_input_tokens,
        },
    );
    Some(Turn {
        ts: entry.timestamp?,
        session: entry.session_id.clone()?,
        agent: entry.agent_id.clone(),
        cwd: entry.cwd.clone().unwrap_or_default(),
        model,
        tokens: Tokens::new(
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
            usage
                .output_tokens_details
                .as_ref()
                .map_or(0, |d| d.thinking_tokens),
        ),
        cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An assistant entry for the call `msg_1`/`req_1`, as logged by the main thread or by a subagent.
    fn call(agent: Option<&str>, output: u64) -> String {
        let agent = agent.map_or("null".to_string(), |a| format!("\"{a}\""));
        format!(
            r#"{{"type":"assistant","sessionId":"s1","agentId":{agent},"timestamp":"2026-10-07T09:24:14Z","requestId":"req_1","message":{{"id":"msg_1","model":"claude-opus-5-5","usage":{{"input_tokens":2,"output_tokens":{output},"cache_read_input_tokens":216747}}}}}}"#
        )
    }

    #[test]
    fn calls_count_once_across_transcripts() {
        // the fork's copy comes first, as the main thread's streamed blocks grow to the final count
        let fork = call(Some("a1"), 1480);
        let main = [call(None, 12), call(None, 1480), call(None, 1480)].join("\n");
        let data = merge(vec![parse(fork.as_bytes()), parse(main.as_bytes())]);

        assert_eq!(data.turns.len(), 1);
        let turn = &data.turns[0];
        assert_eq!(turn.tokens.get(Category::Output), 1480);
        assert_eq!(turn.tokens.get(Category::CacheRead), 216_747);
        assert!(turn.agent.is_none(), "main thread's copy is kept");
    }

    #[test]
    fn invalid_utf8_costs_only_its_line() {
        let mut transcript = b"{\"type\":\"user\",\"text\":\"\xff\xfe\"}\n".to_vec();
        transcript.extend(call(None, 7).as_bytes());
        let data = merge(vec![parse(transcript.as_slice())]);
        assert_eq!(data.turns.len(), 1);
    }
}
