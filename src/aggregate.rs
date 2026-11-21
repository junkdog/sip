use clap::ValueEnum;
use jiff::civil::{self, Weekday};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use rustc_hash::{FxHashMap, FxHashSet};

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
    /// Project name (see [`project_names`]); for days and weeks, empty unless a single project was active;
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
struct Acc<'a> {
    projects: FxHashMap<&'a str, usize>,
    start: Option<Timestamp>,
    tokens: Tokens,
    turns: usize,
    cost: f64,
    unpriced: usize,
    models: FxHashMap<&'a str, usize>,
    peak_main: (u64, u64),
    peak_any: (u64, u64),
}

/// What a row groups by: the day (or the monday of the week), else the project name or session id.
#[derive(PartialEq, Eq, Hash)]
enum Key<'a> {
    Date(civil::Date),
    Name(&'a str),
}

pub fn aggregate(data: &Transcripts, turns: &[&Turn], by: GroupBy) -> Vec<Row> {
    let tz = TimeZone::system();
    let launch_dirs = if by == GroupBy::Project {
        launch_dirs(data)
    } else {
        FxHashMap::default()
    };
    let names = project_names(data.turns.iter().map(|t| &*t.cwd));
    let mut groups: FxHashMap<Key, Acc> = FxHashMap::default();
    let mut days = Days::default();

    for turn in turns {
        let key = match by {
            GroupBy::Day => Key::Date(days.date(turn.ts, &tz)),
            GroupBy::Week => Key::Date(week_start(days.date(turn.ts, &tz))),
            GroupBy::Project => Key::Name(
                &names[launch_dirs
                    .get(&*turn.session)
                    .copied()
                    .unwrap_or(&turn.cwd)],
            ),
            GroupBy::Session => Key::Name(&turn.session),
        };

        let acc = groups.entry(key).or_default();
        *acc.projects.entry(&names[&*turn.cwd]).or_default() += 1;
        acc.start = Some(acc.start.map_or(turn.ts, |s| s.min(turn.ts)));
        acc.tokens += turn.tokens;
        acc.turns += 1;
        match turn.cost {
            Some(c) => acc.cost += c,
            None => acc.unpriced += 1,
        }
        *acc.models.entry(&turn.model).or_default() += 1;

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
            let label = match (by, key) {
                // keyed by the week's monday: `W40 2026-09-28`
                (GroupBy::Week, Key::Date(monday)) => {
                    format!("W{:02} {monday}", monday.iso_week_date().week())
                }
                (GroupBy::Session, _) => start
                    .to_zoned(tz.clone())
                    .strftime("%m-%d %H:%M")
                    .to_string(),
                (_, Key::Date(day)) => day.to_string(),
                (_, Key::Name(project)) => project.to_string(),
            };
            let mut models: Vec<_> = acc
                .models
                .into_iter()
                .map(|(m, n)| (m.to_string(), n))
                .collect();
            models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let (peak_context, window) = if acc.peak_main.0 > 0 {
                acc.peak_main
            } else {
                acc.peak_any
            };
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
        .get(&*turn.session)
        .is_some_and(|models| models.contains(&*turn.model));
    if flagged || ctx > DEFAULT_WINDOW {
        EXTENDED_WINDOW
    } else {
        DEFAULT_WINDOW
    }
}

/// Session -> cwd of its earliest call, so turns made after a `cd` into a
/// subdirectory still count towards the project the session was started in.
pub fn launch_dirs(data: &Transcripts) -> FxHashMap<&str, &str> {
    let mut first: FxHashMap<&str, (Timestamp, &str)> = FxHashMap::default();
    for turn in &data.turns {
        let e = first.entry(&turn.session).or_insert((turn.ts, &turn.cwd));
        if turn.ts < e.0 {
            *e = (turn.ts, &turn.cwd);
        }
    }
    first.into_iter().map(|(s, (_, cwd))| (s, cwd)).collect()
}

/// Cwd -> project name: its basename, prefixed with as many parent directories as it takes
/// to tell it apart from every other cwd, e.g. `jvm/sift` and `rust/sift`.
fn project_names<'a>(cwds: impl Iterator<Item = &'a str>) -> FxHashMap<&'a str, String> {
    let cwds: Vec<(&str, Vec<&str>)> = cwds
        .collect::<FxHashSet<_>>()
        .into_iter()
        .map(|cwd| (cwd, cwd.rsplit('/').filter(|s| !s.is_empty()).collect()))
        .collect();

    cwds.iter()
        .map(|(cwd, parts)| {
            let shared = cwds
                .iter()
                .filter(|(_, other)| other != parts)
                .map(|(_, other)| parts.iter().zip(other).take_while(|(a, b)| a == b).count())
                .max()
                .unwrap_or(0);
            let mut name = parts[..(shared + 1).min(parts.len())].to_vec();
            name.reverse();
            (*cwd, name.join("/"))
        })
        .collect()
}

/// Monday of the ISO week containing `date`.
pub fn week_start(date: civil::Date) -> civil::Date {
    let offset = date.weekday().since(Weekday::Monday);
    date.checked_sub(i64::from(offset).days())
        .expect("date in range")
}

/// Sessions may `cd` around, so they take their most used project.
fn project_label(projects: FxHashMap<&str, usize>, by: GroupBy) -> String {
    // project rows already carry the name as their label
    if by == GroupBy::Project || (by != GroupBy::Session && projects.len() > 1) {
        return String::new();
    }
    projects
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(p, _)| p.to_string())
        .unwrap_or_default()
}

/// The local date of each timestamp, remembering the bounds of the last day it saw: calls come
/// in time order, so most fall on the same day as the one before.
#[derive(Default)]
struct Days {
    last: Option<(Timestamp, Timestamp, civil::Date)>,
}

impl Days {
    fn date(&mut self, ts: Timestamp, tz: &TimeZone) -> civil::Date {
        if let Some((start, end, date)) = self.last
            && start <= ts
            && ts < end
        {
            return date;
        }
        let date = ts.to_zoned(tz.clone()).date();
        // a day that can't be bounded (at the edge of the supported range) just isn't remembered
        let midnight = |d: civil::Date| Some(d.to_zoned(tz.clone()).ok()?.timestamp());
        self.last = midnight(date)
            .zip(date.tomorrow().ok().and_then(midnight))
            .map(|(start, end)| (start, end, date));
        date
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_names_disambiguate_shared_basenames() {
        let cwds = [
            "/home/u/jvm/sift",
            "/home/u/rust/sift",
            "/home/u/rust/sip",
            "/home/u/rust/sip/",
            "/x/u/rust/sip",
            "",
        ];
        let names = project_names(cwds.into_iter());
        assert_eq!(names["/home/u/jvm/sift"], "jvm/sift");
        assert_eq!(names["/home/u/rust/sift"], "rust/sift");
        assert_eq!(names["/home/u/rust/sip"], "home/u/rust/sip");
        assert_eq!(names["/home/u/rust/sip/"], "home/u/rust/sip");
        assert_eq!(names["/x/u/rust/sip"], "x/u/rust/sip");
        assert_eq!(names[""], "");
    }
}
