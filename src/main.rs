mod aggregate;
mod ansi;
mod gnuplot;
mod model;
mod source;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, civil};

use aggregate::GroupBy;
use model::Category;

/// Visualize Claude Code token usage.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Group usage per day or per session
    #[arg(short, long, value_enum, default_value_t = GroupBy::Day)]
    by: GroupBy,

    /// Categories to show (comma separated); all by default
    #[arg(short, long, value_enum, value_delimiter = ',')]
    category: Vec<Category>,

    /// Only include usage since DATE (YYYY-MM-DD) or a relative span (12h, 7d, 2w)
    #[arg(long)]
    since: Option<String>,

    /// Only include usage until DATE (YYYY-MM-DD, inclusive) or a relative span ago
    #[arg(long)]
    until: Option<String>,

    /// Only include sessions whose working directory contains this string
    #[arg(short, long)]
    project: Option<String>,

    /// Only include models containing this string (e.g. opus, sonnet-5)
    #[arg(short, long)]
    model: Option<String>,

    /// Show subagents as separate rows (with --by session)
    #[arg(long)]
    split_agents: bool,

    /// Show only the N most recent rows; 0 shows everything
    #[arg(short = 'n', long, default_value_t = 30)]
    limit: usize,

    /// Renderer
    #[arg(short, long, value_enum, default_value_t = Renderer::Ansi)]
    render: Renderer,

    /// PNG output path for the gnuplot renderer
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Claude Code transcript directory [default: $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects]
    #[arg(long)]
    dir: Option<PathBuf>,

    /// Disable colors
    #[arg(long)]
    no_color: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Renderer {
    Ansi,
    Gnuplot,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dir = cli.dir.clone().unwrap_or_else(source::default_dir);
    let data = source::load(&dir)?;

    let since = cli.since.as_deref().map(|s| parse_time(s, false)).transpose()?;
    let until = cli.until.as_deref().map(|s| parse_time(s, true)).transpose()?;

    let turns: Vec<_> = data
        .turns
        .iter()
        .filter(|t| since.is_none_or(|s| t.ts >= s))
        .filter(|t| until.is_none_or(|u| t.ts < u))
        .filter(|t| cli.project.as_ref().is_none_or(|p| t.cwd.contains(p.as_str())))
        .filter(|t| cli.model.as_ref().is_none_or(|m| t.model.contains(m.as_str())))
        .collect();

    let mut rows = aggregate::aggregate(&data, &turns, cli.by, cli.split_agents);
    if cli.limit > 0 && rows.len() > cli.limit {
        rows.drain(..rows.len() - cli.limit);
    }
    if rows.is_empty() {
        bail!("no token usage found in {}", dir.display());
    }

    let categories = match cli.category.as_slice() {
        [] => Category::ALL.to_vec(),
        c => c.to_vec(),
    };

    match cli.render {
        Renderer::Ansi => {
            let color = !cli.no_color
                && std::env::var_os("NO_COLOR").is_none()
                && std::io::stdout().is_terminal();
            let width = terminal_size::terminal_size().map_or(120, |(w, _)| w.0 as usize);
            let style = ansi::Style { color, width };
            print!("{}", ansi::render(&rows, &categories, &style));
        }
        Renderer::Gnuplot => {
            let output = cli.output.clone().unwrap_or_else(gnuplot::default_output);
            let title = match cli.by {
                GroupBy::Day => "Claude Code token usage per day",
                GroupBy::Session => "Claude Code token usage per session",
            };
            gnuplot::render(&rows, &categories, &output, title)?;
            if !gnuplot::display_inline(&output)? {
                println!("{}", output.display());
            }
        }
    }
    Ok(())
}

/// `2026-09-01` (local midnight; `end` selects the following midnight) or `12h`/`7d`/`2w` ago.
fn parse_time(s: &str, end: bool) -> Result<Timestamp> {
    let tz = TimeZone::system();
    if let Ok(date) = s.parse::<civil::Date>() {
        let date = if end { date.tomorrow()? } else { date };
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
    let now = Timestamp::now().to_zoned(tz);
    Ok(now.checked_sub(hours.hours())?.timestamp())
}
