mod aggregate;
mod ansi;
mod calendar;
mod model;
mod pricing;
mod source;

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use clap_complete::env::Shells;
use clap_complete::{ArgValueCandidates, CompleteEnv, CompletionCandidate, Shell};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, civil};

use aggregate::GroupBy;
use model::Category;

/// Visualize Claude Code token usage.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Group usage per day, week, session or project
    #[arg(short, long, value_enum, default_value_t = GroupBy::Day)]
    by: GroupBy,

    /// Categories to show (comma separated); all by default. --calendar takes a single one and
    /// shades by cost without it
    #[arg(short, long, value_enum, value_delimiter = ',')]
    category: Vec<Category>,

    /// Show every month in range as a calendar, each day shaded by its cost (or by its tokens
    /// of a single --category)
    #[arg(long, visible_alias = "cal", conflicts_with_all = ["by", "limit"])]
    calendar: bool,

    /// Only include usage since DATE (YYYY-MM-DD), today, week (from monday), month (from the 1st),
    /// the most recent day of week (mon..sun) or a relative span (12h, 7d, 2w)
    #[arg(long)]
    since: Option<String>,

    /// Only include usage until DATE (YYYY-MM-DD, inclusive), the end of today/week/month or
    /// the most recent day of week (mon..sun), or a relative span ago
    #[arg(long)]
    until: Option<String>,

    /// Only include sessions started in a directory whose path contains this string
    #[arg(short, long)]
    project: Option<String>,

    /// Only include models containing this string (e.g. opus, sonnet-5)
    #[arg(short, long, add = ArgValueCandidates::new(model_candidates))]
    model: Option<String>,

    /// Show only the N most recent rows; 0 shows everything
    #[arg(short = 'n', long, default_value_t = 30)]
    limit: usize,

    /// Claude Code transcript directory [default: $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects]
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    dir: Option<PathBuf>,

    /// Disable colors
    #[arg(long)]
    no_color: bool,

    /// Print a shell completion script and exit (e.g. `source <(sip --completions zsh)`)
    #[arg(long, value_name = "SHELL", exclusive = true)]
    completions: Option<Shell>,
}

fn main() -> Result<()> {
    // the registered completion script calls back into sip with COMPLETE=<shell> set
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();
    if let Some(shell) = cli.completions {
        let completer = std::env::current_exe().context("locating the sip binary")?;
        Shells::builtins()
            .completer(&shell.to_string())
            .context("unsupported shell")?
            .write_registration(
                "COMPLETE",
                "sip",
                "sip",
                &completer.to_string_lossy(),
                &mut std::io::stdout(),
            )?;
        return Ok(());
    }

    let dir = cli.dir.clone().unwrap_or_else(source::default_dir);
    let data = source::load(&dir)?;
    source::remember_models(&data);

    let since = cli
        .since
        .as_deref()
        .map(|s| parse_time(s, false))
        .transpose()?;
    let until = cli
        .until
        .as_deref()
        .map(|s| parse_time(s, true))
        .transpose()?;

    if data.turns.is_empty() {
        bail!("no token usage found in {}", dir.display());
    }

    // a session belongs to the project it was started in, as with --by project, even after a `cd`
    let launch_dirs = if cli.project.is_some() {
        aggregate::launch_dirs(&data)
    } else {
        Default::default()
    };
    let turns: Vec<_> = data
        .turns
        .iter()
        .filter(|t| since.is_none_or(|s| t.ts >= s))
        .filter(|t| until.is_none_or(|u| t.ts < u))
        .filter(|t| {
            cli.project.as_ref().is_none_or(|p| {
                let dir = launch_dirs
                    .get(t.session.as_str())
                    .copied()
                    .unwrap_or(t.cwd.as_str());
                dir.contains(p.as_str())
            })
        })
        .filter(|t| {
            cli.model
                .as_ref()
                .is_none_or(|m| t.model.contains(m.as_str()))
        })
        .collect();

    let by = if cli.calendar { GroupBy::Day } else { cli.by };
    let mut rows = aggregate::aggregate(&data, &turns, by);
    if rows.is_empty() {
        bail!("no token usage matches --since, --until, --project or --model");
    }

    let color =
        !cli.no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
    // without a terminal on any std stream (e.g. under a pager), $COLUMNS when it is exported
    let width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(120);
    let style = ansi::Style { color, width };

    if cli.calendar {
        let metric = match cli.category.as_slice() {
            [] => calendar::Metric::Cost,
            [c] => calendar::Metric::Tokens(*c),
            _ => bail!("--calendar shades by a single category"),
        };
        let range = calendar_range(&rows, since, until)?;
        print!("{}", calendar::render(&rows, metric, range, &style));
        return Ok(());
    }

    if cli.limit > 0 && rows.len() > cli.limit {
        rows.drain(..rows.len() - cli.limit);
    }

    let categories = match cli.category.as_slice() {
        [] => Category::ALL.to_vec(),
        c => c.to_vec(),
    };

    print!("{}", ansi::render(&rows, &categories, cli.by, &style));
    Ok(())
}

/// First and last day (inclusive) of the calendar: those of `--since`/`--until` when given,
/// otherwise from the first day with usage through today.
fn calendar_range(
    rows: &[aggregate::Row],
    since: Option<Timestamp>,
    until: Option<Timestamp>,
) -> Result<(civil::Date, civil::Date)> {
    let tz = TimeZone::system();
    let date = |ts: Timestamp| ts.to_zoned(tz.clone()).date();
    let first = date(since.unwrap_or(rows[0].start));
    let last = match until {
        // the bound is exclusive: a midnight ends the day before it
        Some(u) => date(u.checked_sub(1.nanosecond())?),
        None => date(Timestamp::now()).max(date(rows[rows.len() - 1].start)),
    };
    Ok((first, last))
}

/// `2026-09-01`, `today`, `week`, `month` or `mon`..`sun` (the most recent such day, today
/// included) as local midnight starting the period (`end` selects the midnight ending it), or
/// `12h`/`7d`/`2w` ago.
fn parse_time(s: &str, end: bool) -> Result<Timestamp> {
    let tz = TimeZone::system();
    let now = Timestamp::now().to_zoned(tz.clone());
    let today = now.date();
    let period = match s {
        "today" => Some((today, today.tomorrow()?)),
        "week" => {
            let monday = aggregate::week_start(today);
            Some((monday, monday.checked_add(1.week())?))
        }
        "month" => {
            let first = today.first_of_month();
            Some((first, first.checked_add(1.month())?))
        }
        _ => match (weekday(s), s.parse::<civil::Date>()) {
            (Some(weekday), _) => {
                let offset = today.weekday().since(weekday);
                let date = today.checked_sub(i64::from(offset).days())?;
                Some((date, date.tomorrow()?))
            }
            (None, Ok(date)) => Some((date, date.tomorrow()?)),
            (None, Err(_)) => None,
        },
    };
    if let Some((start, next)) = period {
        let date = if end { next } else { start };
        return Ok(date.to_zoned(tz)?.timestamp());
    }

    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse().with_context(|| format!("invalid time: {s}"))?;
    let hours = match unit {
        "h" => n,
        "d" | "" => n * 24,
        "w" => n * 24 * 7,
        _ => bail!("invalid time unit in {s}; expected h, d or w"),
    };
    Ok(now.checked_sub(hours.hours())?.timestamp())
}

/// `--model` completions: the models recorded by earlier runs, or before the first one, those in
/// the default transcript directory (`--dir` is not visible to completers).
fn model_candidates() -> Vec<CompletionCandidate> {
    let models = match source::cached_models() {
        Some(models) => models,
        None => {
            let Ok(data) = source::load(&source::default_dir()) else {
                return Vec::new();
            };
            source::remember_models(&data);
            data.turns
                .into_iter()
                .map(|t| t.model)
                .collect::<BTreeSet<_>>()
        }
    };
    models.into_iter().map(CompletionCandidate::new).collect()
}

fn weekday(s: &str) -> Option<civil::Weekday> {
    use civil::Weekday::*;
    Some(match s {
        "mon" => Monday,
        "tue" => Tuesday,
        "wed" => Wednesday,
        "thu" => Thursday,
        "fri" => Friday,
        "sat" => Saturday,
        "sun" => Sunday,
        _ => return None,
    })
}
