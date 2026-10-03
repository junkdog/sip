use std::collections::HashMap;

use clap::ValueEnum;
use jiff::civil::{self, Weekday};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

use crate::model::{Tokens, short_model};
use crate::source::{Transcripts, Turn};

const DEFAULT_WINDOW: u64 = 200_000;
const EXTENDED_WINDOW: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GroupBy {
    Day,
    Week,
    Session,
    Project,
}

#[derive(Debug)]
pub struct Row {
    pub label: String,
    /// Project name (cwd basename); for days and weeks, empty unless a single project was active;
    /// always empty for projects, whose label is the name.
    pub project: String,
    pub start: Timestamp,
    pub tokens: Tokens,
    pub turns: usize,
    /// Estimated USD across the calls with a known price.
    pub cost: f64,
    /// Calls whose model has no known price, so `cost` is a lower bound.
    pub unpriced: usize,
    /// Model -> number of calls, most used first.
    pub models: Vec<(String, usize)>,
    /// Largest context seen on a single call (main thread preferred over subagents).
    pub peak_context: u64,
    /// Context window of the peak call's session; `None` for days, weeks and projects, which span sessions.
    pub window: Option<u64>,
}

impl Row {
    pub fn model_label(&self) -> String {
        match self.models.as_slice() {
            [] => String::new(),
            [(m, _)] => short_model(m),
            [(m, _), rest @ ..] => format!("{}+{}", short_model(m), rest.len()),
        }
    }
}

#[derive(Default)]
struct Acc {
    projects: HashMap<String, usize>,
    start: Option<Timestamp>,
    tokens: Tokens,
    turns: usize,
    cost: f64,
    unpriced: usize,
    models: HashMap<String, usize>,
    peak_main: (u64, u64),
    peak_any: (u64, u64),
}

pub fn aggregate(data: &Transcripts, turns: &[&Turn], by: GroupBy, split_subagents: bool) -> Vec<Row> {
    let tz = TimeZone::system();
    let launch_dirs = if by == GroupBy::Project { launch_dirs(data) } else { HashMap::new() };
    let mut groups: HashMap<String, Acc> = HashMap::new();

    for turn in turns {
        let key = match by {
            GroupBy::Day => turn.ts.to_zoned(tz.clone()).date().to_string(),
            GroupBy::Week => week_start(turn.ts.to_zoned(tz.clone()).date()).to_string(),
            GroupBy::Project => project_name(launch_dirs.get(turn.session.as_str()).copied().unwrap_or(&turn.cwd)),
            GroupBy::Session => match (&turn.agent, split_subagents) {
                (Some(agent), true) => format!("{}/{agent}", turn.session),
                _ => turn.session.clone(),
            },
        };

        let acc = groups.entry(key).or_default();
        *acc.projects.entry(project_name(&turn.cwd)).or_default() += 1;
        acc.start = Some(acc.start.map_or(turn.ts, |s| s.min(turn.ts)));
        acc.tokens += turn.tokens;
        acc.turns += 1;
        match turn.cost {
            Some(c) => acc.cost += c,
            None => acc.unpriced += 1,
        }
        *acc.models.entry(turn.model.clone()).or_default() += 1;

        let ctx = turn.tokens.context();
        let window = window_for(data, turn, ctx);
        if turn.agent.is_none() && ctx > acc.peak_main.0 {
            acc.peak_main = (ctx, window);
        }
        if ctx > acc.peak_any.0 {
            acc.peak_any = (ctx, window);
        }
    }

    let mut rows: Vec<Row> = groups
        .into_iter()
        .map(|(key, acc)| {
            let start = acc.start.expect("group has at least one turn");
            let label = match by {
                GroupBy::Day => key,
                // keyed by the week's monday: `W40 2026-09-28`
                GroupBy::Week => {
                    let monday: civil::Date = key.parse().expect("week key is a date");
                    format!("W{:02} {key}", monday.iso_week_date().week())
                }
                GroupBy::Project => key,
                GroupBy::Session => {
                    let time = start.to_zoned(tz.clone()).strftime("%m-%d %H:%M").to_string();
                    // split-out subagent rows are keyed "<session>/<agent>"
                    if key.contains('/') { format!("{time} ↳") } else { format!("{time}  ") }
                }
            };
            let mut models: Vec<_> = acc.models.into_iter().collect();
            models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let (peak_context, window) = if acc.peak_main.0 > 0 { acc.peak_main } else { acc.peak_any };
            Row {
                label,
                project: project_label(acc.projects, by),
                start,
                tokens: acc.tokens,
                turns: acc.turns,
                cost: acc.cost,
                unpriced: acc.unpriced,
                models,
                peak_context,
                window: (by == GroupBy::Session).then_some(window),
            }
        })
        .collect();

    match by {
        // most expensive last, so `--limit` keeps the top spenders
        GroupBy::Project => rows.sort_by(|a, b| a.cost.total_cmp(&b.cost)),
        _ => rows.sort_by_key(|r| r.start),
    }
    rows
}

/// The 1M window is recorded per session in `cost-state` entries; older
/// transcripts lack those, so a call exceeding 200k also implies 1M.
fn window_for(data: &Transcripts, turn: &Turn, ctx: u64) -> u64 {
    let flagged = data
        .one_million
        .get(&turn.session)
        .is_some_and(|models| models.contains(&turn.model));
    if flagged || ctx > DEFAULT_WINDOW { EXTENDED_WINDOW } else { DEFAULT_WINDOW }
}

/// Session -> cwd of its earliest call, so turns made after a `cd` into a
/// subdirectory still count towards the project the session was started in.
fn launch_dirs(data: &Transcripts) -> HashMap<&str, &str> {
    let mut first: HashMap<&str, (Timestamp, &str)> = HashMap::new();
    for turn in &data.turns {
        let e = first.entry(&turn.session).or_insert((turn.ts, &turn.cwd));
        if turn.ts < e.0 {
            *e = (turn.ts, &turn.cwd);
        }
    }
    first.into_iter().map(|(s, (_, cwd))| (s, cwd)).collect()
}

fn project_name(cwd: &str) -> String {
    cwd.rsplit('/').find(|s| !s.is_empty()).unwrap_or(cwd).to_string()
}

/// Monday of the ISO week containing `date`.
pub fn week_start(date: civil::Date) -> civil::Date {
    let offset = date.weekday().since(Weekday::Monday);
    date.checked_sub(i64::from(offset).days()).expect("date in range")
}

/// Sessions may `cd` around, so they take their most used project.
fn project_label(projects: HashMap<String, usize>, by: GroupBy) -> String {
    // project rows already carry the name as their label
    if by == GroupBy::Project || (by != GroupBy::Session && projects.len() > 1) {
        return String::new();
    }
    projects
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
        .map(|(p, _)| p)
        .unwrap_or_default()
}
