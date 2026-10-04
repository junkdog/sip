//! Reads Claude Code session transcripts (`~/.claude/projects/**/*.jsonl`).

use std::collections::{HashMap, HashSet};
use std::fs::File;
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
    anyhow::ensure!(dir.is_dir(), "transcript directory not found: {}", dir.display());

    let files: Vec<PathBuf> = WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| e.into_path())
        .collect();

    let parsed: Vec<Transcripts> = files.par_iter().map(|f| parse_file(f)).collect();

    let mut out = Transcripts::default();
    for t in parsed {
        out.turns.extend(t.turns);
        for (session, models) in t.one_million {
            out.one_million.entry(session).or_default().extend(models);
        }
    }
    out.turns.sort_by_key(|t| t.ts);
    Ok(out)
}

fn parse_file(path: &Path) -> Transcripts {
    let mut out = Transcripts::default();
    let Ok(file) = File::open(path) else {
        return out;
    };
    let mut seen: HashMap<(String, String), usize> = HashMap::new();

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let is_assistant = line.contains(r#""usage""#) && line.contains(r#""type":"assistant""#);
        let is_cost = line.contains(r#""type":"cost-state""#);
        if !is_assistant && !is_cost {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            continue;
        };

        match entry.kind.as_str() {
            "cost-state" => {
                let (Some(session), Some(models)) = (entry.session_id, entry.model_usage) else {
                    continue;
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
                let Some(turn) = to_turn(&entry) else { continue };
                // streamed responses are logged once per content block, all sharing
                // the same message id + request id; keep the most complete copy
                let key = entry
                    .message
                    .as_ref()
                    .and_then(|m| m.id.clone())
                    .zip(entry.request_id.clone());
                match key {
                    Some(key) => match seen.get(&key) {
                        Some(&i) => {
                            let prev = &mut out.turns[i];
                            if turn.tokens.get(Category::Output) > prev.tokens.get(Category::Output) {
                                *prev = turn;
                            }
                        }
                        None => {
                            seen.insert(key, out.turns.len());
                            out.turns.push(turn);
                        }
                    },
                    None => out.turns.push(turn),
                }
            }
            _ => {}
        }
    }
    out
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
    let cost = pricing::cost(&model, &Billed {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_write_5m,
        cache_write_1h,
        cache_read: usage.cache_read_input_tokens,
    });
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
            usage.output_tokens_details.as_ref().map_or(0, |d| d.thinking_tokens),
        ),
        cost,
    })
}
