//! Reads Claude Code session transcripts (`~/.claude/projects/**/*.jsonl`).

use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, hash_map};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, mpsc};

use anyhow::Result;
use jiff::Timestamp;
use memchr::memmem::Finder;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use serde::de::IgnoredAny;
use walkdir::WalkDir;

use crate::model::{Category, Tokens};
use crate::pricing::{self, Usage as Billed};

/// One deduplicated assistant API call.
#[derive(Debug, Clone)]
pub struct Turn {
    pub ts: Timestamp,
    pub session: Arc<str>,
    /// Set for subagent (sidechain) transcripts.
    pub agent: Option<Arc<str>>,
    pub cwd: Arc<str>,
    pub model: Arc<str>,
    pub tokens: Tokens,
    /// Estimated USD; `None` when the model has no known price.
    pub cost: Option<f64>,
}

#[derive(Debug, Default)]
pub struct Transcripts {
    pub turns: Vec<Turn>,
    /// Session id -> models that ran with the 1M context window (`[1m]` suffix stripped).
    pub one_million: FxHashMap<String, FxHashSet<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    session_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    agent_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    cwd: Option<Cow<'a, str>>,
    timestamp: Option<Timestamp>,
    #[serde(borrow)]
    request_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    message: Option<Message<'a>>,
    model_usage: Option<HashMap<String, IgnoredAny>>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    model: Option<Cow<'a, str>>,
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

/// Bytes read per call. 256 KiB to 1 MiB measured alike, cold or warm; 4 MiB was slower warm
/// (it fits the CPU caches less well), 16 MiB slower cold, and both took more memory.
const CHUNK: usize = 1 << 20;

thread_local! {
    /// Reused across the files a thread reads, so each file doesn't fault in a fresh buffer.
    static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
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

    // merged as each transcript is parsed: on a cold cache, this thread works while the rest wait on the disk
    let mut merged = Merge::default();
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        let files = &files;
        s.spawn(move || {
            files
                .par_iter()
                .enumerate()
                .for_each_with(tx, |tx, (i, path)| {
                    let Ok(file) = File::open(path) else { return };
                    let mut parsed = BUF.with_borrow_mut(|buf| parse_with(file, buf));
                    // freed here rather than by the merging thread
                    drop(std::mem::take(&mut parsed.index));
                    let _ = tx.send((i, parsed));
                });
        });
        for (i, parsed) in rx {
            merged.add(i, parsed);
        }
    });
    Ok(merged.finish())
}

/// Message id + request id (see [`call_id`]): the same API call, in whichever transcript it was logged.
type CallId = Box<str>;

/// One transcript's calls, deduplicated within the transcript but not yet against the others.
#[derive(Default)]
struct Parsed {
    calls: Vec<(Option<CallId>, Turn)>,
    /// Position in `calls` of each call id.
    index: FxHashMap<CallId, usize>,
    one_million: FxHashMap<String, FxHashSet<String>>,
    /// The latest of each string a turn holds: consecutive lines nearly always repeat them.
    session: Option<Arc<str>>,
    agent: Option<Arc<str>>,
    cwd: Option<Arc<str>>,
    model: Option<Arc<str>>,
    /// Scratch for building call ids.
    key: String,
}

impl Parsed {
    /// Adds a copy of a call, or keeps whichever of it and an earlier copy [`supersedes`].
    fn add(&mut self, id: Option<(&str, &str)>, turn: Turn) {
        let Some((message, request)) = id else {
            self.calls.push((None, turn));
            return;
        };
        call_id(&mut self.key, message, request);
        match self.index.get(self.key.as_str()) {
            Some(&i) if supersedes(&turn, &self.calls[i].1) => self.calls[i].1 = turn,
            Some(_) => {}
            None => {
                let id: CallId = self.key.as_str().into();
                self.index.insert(id.clone(), self.calls.len());
                self.calls.push((Some(id), turn));
            }
        }
    }
}

/// `message` and `request` joined unambiguously, without assuming which characters ids avoid.
fn call_id(out: &mut String, message: &str, request: &str) {
    out.clear();
    let _ = write!(out, "{}:{message}{request}", message.len());
}

/// Returns the shared copy of `s`, reusing the one in `slot` when it is the same string.
fn intern(slot: &mut Option<Arc<str>>, s: &str) -> Arc<str> {
    match slot {
        Some(kept) if **kept == *s => kept.clone(),
        _ => slot.insert(Arc::from(s)).clone(),
    }
}

/// Streamed responses are logged once per content block, and a forked subagent opens its own
/// transcript with a copy of the parent's last call; all copies share the call's id. Keeps the
/// most complete copy of each call, and the main thread's over a subagent's on a tie; on a full
/// tie, the copy in the earliest transcript (in directory order). [`Parsed::add`] already did so
/// within each transcript.
///
/// Transcripts may arrive in any order, and the result is the same for every one of them.
#[derive(Default)]
struct Merge {
    calls: Vec<Merged>,
    seen: FxHashMap<CallId, usize>,
    one_million: FxHashMap<String, FxHashSet<String>>,
}

struct Merged {
    /// Transcript and position of the call's first copy: orders calls made in the same millisecond.
    first: (u32, u32),
    /// Transcript of the kept copy.
    file: u32,
    turn: Turn,
}

impl Merge {
    /// Adds the calls of the `file`th transcript.
    fn add(&mut self, file: usize, parsed: Parsed) {
        let file = file as u32;
        for (pos, (id, turn)) in parsed.calls.into_iter().enumerate() {
            let first = (file, pos as u32);
            let Some(id) = id else {
                self.calls.push(Merged { first, file, turn });
                continue;
            };
            match self.seen.entry(id) {
                hash_map::Entry::Occupied(e) => {
                    let kept = &mut self.calls[*e.get()];
                    kept.first = kept.first.min(first);
                    let tie = !supersedes(&kept.turn, &turn);
                    if supersedes(&turn, &kept.turn) || (tie && file < kept.file) {
                        kept.file = file;
                        kept.turn = turn;
                    }
                }
                hash_map::Entry::Vacant(e) => {
                    e.insert(self.calls.len());
                    self.calls.push(Merged { first, file, turn });
                }
            }
        }
        for (session, models) in parsed.one_million {
            self.one_million.entry(session).or_default().extend(models);
        }
    }

    fn finish(self) -> Transcripts {
        let mut calls = self.calls;
        calls.par_sort_unstable_by_key(|c| (c.turn.ts, c.first));
        Transcripts {
            turns: calls.into_iter().map(|c| c.turn).collect(),
            one_million: self.one_million,
        }
    }
}

/// Merges transcripts given in directory order.
#[cfg(test)]
fn merge(parsed: Vec<Parsed>) -> Transcripts {
    let mut merged = Merge::default();
    for (i, p) in parsed.into_iter().enumerate() {
        merged.add(i, p);
    }
    merged.finish()
}

fn supersedes(copy: &Turn, kept: &Turn) -> bool {
    let output = |t: &Turn| t.tokens.get(Category::Output);
    match output(copy).cmp(&output(kept)) {
        Ordering::Greater => true,
        Ordering::Equal => copy.agent.is_none() && kept.agent.is_some(),
        Ordering::Less => false,
    }
}

static USAGE: LazyLock<Finder> = LazyLock::new(|| Finder::new(r#""usage""#));
static ASSISTANT: LazyLock<Finder> = LazyLock::new(|| Finder::new(r#""type":"assistant""#));
static COST_STATE: LazyLock<Finder> = LazyLock::new(|| Finder::new(r#""type":"cost-state""#));

#[cfg(test)]
fn parse(reader: impl Read) -> Parsed {
    parse_with(reader, &mut Vec::new())
}

/// Reads `reader` through `buf`, a chunk at a time; a line which outgrows the chunk grows `buf`.
fn parse_with(mut reader: impl Read, buf: &mut Vec<u8>) -> Parsed {
    let mut out = Parsed::default();
    if buf.is_empty() {
        buf.resize(CHUNK, 0);
    }
    // bytes of a line still waiting for the rest of it, at the start of `buf`
    let mut pending = 0;
    loop {
        if pending == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
        let read = match reader.read(&mut buf[pending..]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // the unfinished line is lost, as are the rest of the file's
            Err(_) => return out,
        };
        let end = pending + read;
        match memchr::memrchr(b'\n', &buf[pending..end]) {
            Some(i) => {
                let cut = pending + i + 1;
                parse_lines(&buf[..cut], &mut out);
                buf.copy_within(cut..end, 0);
                pending = end - cut;
            }
            None => pending = end,
        }
    }
    parse_lines(&buf[..pending], &mut out);
    out
}

/// Filtered as bytes: only the ~quarter of lines that are parsed pay for UTF-8 validation.
fn parse_lines(bytes: &[u8], out: &mut Parsed) {
    let mut start = 0;
    for end in memchr::memchr_iter(b'\n', bytes).chain([bytes.len()]) {
        parse_line(&bytes[start..end], out);
        start = end + 1;
    }
}

fn parse_line(line: &[u8], out: &mut Parsed) {
    let is_assistant = USAGE.find(line).is_some() && ASSISTANT.find(line).is_some();
    if !is_assistant && COST_STATE.find(line).is_none() {
        return;
    }
    match serde_json::from_slice::<Entry>(line) {
        Ok(entry) => add_entry(entry, out),
        // a line which isn't valid UTF-8 is read lossily, rather than dropped
        Err(_) => {
            let line = String::from_utf8_lossy(line);
            if let Ok(entry) = serde_json::from_str::<Entry>(&line) {
                add_entry(entry, out);
            }
        }
    }
}

fn add_entry(entry: Entry, out: &mut Parsed) {
    match &*entry.kind {
        "cost-state" => {
            let (Some(session), Some(models)) = (entry.session_id, entry.model_usage) else {
                return;
            };
            let big: FxHashSet<String> = models
                .into_keys()
                .filter_map(|m| m.strip_suffix("[1m]").map(str::to_string))
                .collect();
            if !big.is_empty() {
                out.one_million
                    .entry(session.into_owned())
                    .or_default()
                    .extend(big);
            }
        }
        "assistant" => {
            let Some(turn) = to_turn(&entry, out) else {
                return;
            };
            let message_id = entry.message.as_ref().and_then(|m| m.id.as_deref());
            out.add(message_id.zip(entry.request_id.as_deref()), turn);
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
    let seen: FxHashSet<&str> = data.turns.iter().map(|t| &*t.model).collect();
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

/// Its strings are interned through the slots in `out`.
fn to_turn(entry: &Entry, out: &mut Parsed) -> Option<Turn> {
    let message = entry.message.as_ref()?;
    let usage = message.usage.as_ref()?;
    let model = message.model.as_deref()?;
    if model.starts_with('<') {
        return None; // "<synthetic>" placeholder messages
    }
    let ts = entry.timestamp?;
    let session = entry.session_id.as_deref()?;
    // without the TTL breakdown, cache writes are assumed to be the default 5 minutes
    let (cache_write_5m, cache_write_1h) = match &usage.cache_creation {
        Some(c) => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
        None => (usage.cache_creation_input_tokens, 0),
    };
    let cost = pricing::cost(
        model,
        &Billed {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_write_5m,
            cache_write_1h,
            cache_read: usage.cache_read_input_tokens,
        },
    );
    Some(Turn {
        ts,
        session: intern(&mut out.session, session),
        agent: entry.agent_id.as_deref().map(|a| intern(&mut out.agent, a)),
        cwd: intern(&mut out.cwd, entry.cwd.as_deref().unwrap_or_default()),
        model: intern(&mut out.model, model),
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
    fn arrival_order_changes_nothing() {
        // two forks hold equally complete copies: the earlier transcript's is kept
        let files = [
            call(Some("a1"), 9),
            call(Some("a2"), 9),
            call(Some("a3"), 3),
        ];
        let in_order = |order: &[usize]| {
            let mut merged = Merge::default();
            for &i in order {
                merged.add(i, parse(files[i].as_bytes()));
            }
            let data = merged.finish();
            assert_eq!(data.turns.len(), 1);
            data.turns[0].agent.as_deref().map(str::to_string)
        };
        for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0], [2, 0, 1]] {
            assert_eq!(in_order(&order).as_deref(), Some("a1"), "order {order:?}");
        }
    }

    #[test]
    fn lines_survive_chunk_boundaries() {
        // distinct calls, each longer than the buffer, the last without a trailing newline
        let transcript: String = (1..=3)
            .map(|n| call(None, n).replace("msg_1", &format!("msg_{n}")))
            .collect::<Vec<_>>()
            .join("\n");
        let mut small = vec![0; 7];
        let data = merge(vec![parse_with(transcript.as_bytes(), &mut small)]);
        let outputs: Vec<_> = data
            .turns
            .iter()
            .map(|t| t.tokens.get(Category::Output))
            .collect();
        assert_eq!(outputs, [1, 2, 3]);
    }

    #[test]
    fn invalid_utf8_costs_only_its_line() {
        let mut transcript = b"{\"type\":\"user\",\"text\":\"\xff\xfe\"}\n".to_vec();
        transcript.extend(call(None, 7).as_bytes());
        let data = merge(vec![parse(transcript.as_slice())]);
        assert_eq!(data.turns.len(), 1);
    }
}
