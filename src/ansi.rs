//! Terminal renderer: one row per group, one fractional-block bar per category.
//! Each category column is scaled to its own maximum, so bars compare rows, not categories.

use std::fmt::Write;

use jiff::tz::TimeZone;

use crate::aggregate::{GroupBy, Row};
use crate::model::{Category, Rgb, Tokens, gruvbox, human};
use crate::pricing::usd;

const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
const NUM_WIDTH: usize = 6;
const MAX_BAR: usize = 40;
/// Bars narrow to this width before the project names start giving up theirs.
const SHRINK_BAR: usize = 4;
/// Narrowest bar, reached once the project names are down to `MIN_NAME`.
const MIN_BAR: usize = 1;
/// Project names keep this many columns, unless they are shorter.
const MIN_NAME: usize = 8;

/// Context utilization color stops: quiet while there's headroom, loud as the window fills.
const UTILIZATION: [(f64, Rgb); 5] = [
    (0.0, gruvbox::GRAY),
    (0.3, gruvbox::GREEN),
    (0.6, gruvbox::YELLOW),
    (0.8, gruvbox::ORANGE),
    (1.0, gruvbox::RED),
];

pub struct Style {
    pub color: bool,
    pub width: usize,
}

impl Style {
    pub fn fg(&self, c: Rgb) -> String {
        if self.color { format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2) } else { String::new() }
    }

    pub fn bg(&self, c: Rgb) -> String {
        if self.color { format!("\x1b[48;2;{};{};{}m", c.0, c.1, c.2) } else { String::new() }
    }

    pub fn bold(&self) -> &'static str {
        if self.color { "\x1b[1m" } else { "" }
    }

    pub fn underline(&self) -> &'static str {
        if self.color { "\x1b[4m" } else { "" }
    }

    pub fn reset(&self) -> &'static str {
        if self.color { "\x1b[0m" } else { "" }
    }

    pub fn paint(&self, c: Rgb, s: &str) -> String {
        format!("{}{s}{}", self.fg(c), self.reset())
    }
}

pub fn render(rows: &[Row], categories: &[Category], by: GroupBy, style: &Style) -> String {
    let label_w = rows.iter().map(|r| r.label.chars().count()).max().unwrap_or(0).max(5);
    let project_w = rows.iter().map(|r| r.project.chars().count()).max().unwrap_or(0).min(20);
    let model_w = rows.iter().map(|r| r.model_label().chars().count()).max().unwrap_or(0).max(5);
    let ctx_w = 9; // "1.0M/1.0M"
    let total_cost: f64 = rows.iter().map(|r| r.cost).sum();
    let cost_w = rows
        .iter()
        .map(|r| r.cost)
        .chain([total_cost])
        .map(|c| usd(c).len())
        .max()
        .unwrap_or(0)
        .max(4)
        + 1; // trailing '?' marks unpriced calls

    // a category cell is bar + space + number, with a gap before the next one
    let cell = |bar: usize| bar + 1 + NUM_WIDTH;
    let cells_w = |n: usize, bar: usize| (n * (cell(bar) + 2)).saturating_sub(2);

    // as the terminal narrows, the bars give way first, then the project names, then the
    // bars down to their minimum, and finally the trailing categories
    let names_are_labels = by == GroupBy::Project;
    let natural_name_w = if names_are_labels { label_w } else { project_w };
    let mut fixed = model_w + 2 + ctx_w + 2 + cost_w + 2;
    if !names_are_labels {
        fixed += label_w + 2;
    }
    let budget = style.width.saturating_sub(fixed + 2 + cells_w(categories.len(), SHRINK_BAR));
    let name_w = natural_name_w.min(budget.max(MIN_NAME));
    if name_w > 0 {
        fixed += name_w + 2;
    }
    let (label_w, project_w) = if names_are_labels { (name_w, project_w) } else { (label_w, name_w) };

    let room = style.width.saturating_sub(fixed) + 2;
    let shown = (room / (cell(MIN_BAR) + 2)).max(1).min(categories.len());
    let categories = &categories[..shown];
    let bar_w = (room / shown.max(1)).saturating_sub(NUM_WIDTH + 3).clamp(MIN_BAR, MAX_BAR);

    let max: Vec<u64> = categories
        .iter()
        .map(|&c| rows.iter().map(|r| r.tokens.get(c)).max().unwrap_or(0))
        .collect();

    let table_w = fixed + cells_w(categories.len(), bar_w);
    let tz = TimeZone::system();
    let mut prev_day = None;

    let mut out = String::new();

    // header
    let mut header = format!("{:label_w$}  ", "");
    if project_w > 0 {
        let _ = write!(header, "{:project_w$}  ", "");
    }
    let ctx_header = if rows.iter().any(|r| r.window.is_some()) { "context" } else { "peak ctx" };
    let _ = write!(header, "{:model_w$}  {:>ctx_w$}  {:>cost_w$}  ", "model", ctx_header, "cost ");
    let head = header.clone();
    out.push_str(&style.paint(gruvbox::GRAY, &head));
    let cell_w = cell(bar_w);
    for (i, &c) in categories.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let label = c.label();
        // the output bar leads with its thinking share; name the dimmer shade when there's room
        let legend = " thinking";
        if c == Category::Output && label.len() + legend.len() <= cell_w {
            out.push_str(&style.paint(c.color(), label));
            out.push_str(&style.paint(Category::thinking_color(), &format!("{legend:<w$}", w = cell_w - label.len())));
        } else {
            let label = truncate(label, cell_w);
            out.push_str(&style.paint(c.color(), &format!("{label:<cell_w$}")));
        }
    }
    out.push('\n');

    for row in rows {
        if by == GroupBy::Session {
            let day = row.start.to_zoned(tz.clone()).date();
            if prev_day != Some(day) {
                prev_day = Some(day);
                out.push_str(&day_separator(&day.strftime("%a %Y-%m-%d").to_string(), table_w, style));
            }
        }

        if by == GroupBy::Project {
            out.push_str(&project_cell(&truncate_start(&row.label, label_w), label_w, (gruvbox::GRAY, gruvbox::FG), style));
        } else {
            out.push_str(&style.paint(gruvbox::FG, &format!("{:label_w$}", row.label)));
        }
        out.push_str("  ");
        if project_w > 0 {
            let name = truncate_start(&row.project, project_w);
            out.push_str(&project_cell(&name, project_w, (gruvbox::NEUTRAL_PURPLE, gruvbox::PURPLE), style));
            out.push_str("  ");
        }
        out.push_str(&style.paint(gruvbox::FG4, &format!("{:model_w$}  ", row.model_label())));
        out.push_str(&context_cell(row, ctx_w, style));
        out.push_str("  ");
        out.push_str(&cost_cell(row.cost, row.unpriced > 0, cost_w, style));
        out.push_str("  ");

        for (i, (&c, &m)) in categories.iter().zip(&max).enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let v = row.tokens.get(c);
            let thinking = if c == Category::Output { row.tokens.thinking() } else { 0 };
            out.push_str(&bar(v, thinking, m, bar_w, c.color(), style));
            out.push(' ');
            out.push_str(&style.paint(gruvbox::FG, &format!("{:>NUM_WIDTH$}", human(v))));
        }
        out.push('\n');
    }

    // totals
    let mut total = Tokens::default();
    rows.iter().for_each(|r| total += r.tokens);
    let turns: usize = rows.iter().map(|r| r.turns).sum();
    let pad = fixed.saturating_sub(cost_w + 4);
    let summary = truncate(&format!("{} rows, {} calls", rows.len(), turns), pad);
    out.push_str(&style.paint(gruvbox::GRAY, &format!("{summary:>pad$}  ")));
    let unpriced = rows.iter().any(|r| r.unpriced > 0);
    out.push_str(&cost_cell(total_cost, unpriced, cost_w, style));
    out.push_str("  ");
    for (i, &c) in categories.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let sum = human(total.get(c));
        let thinking = human(total.thinking());
        if c == Category::Output && total.thinking() > 0 && thinking.len() + 1 + sum.len() <= cell_w {
            let w = cell_w - sum.len() - 1;
            out.push_str(&style.paint(Category::thinking_color(), &format!("{thinking:>w$} ")));
            out.push_str(&style.paint(c.color(), &sum));
        } else {
            out.push_str(&style.paint(c.color(), &format!("{sum:>cell_w$}")));
        }
    }
    out.push('\n');
    fit(&out, style.width)
}

/// `value` scaled against `max`, with its leading `thinking` share drawn in the thinking shade.
fn bar(value: u64, thinking: u64, max: u64, width: usize, color: Rgb, style: &Style) -> String {
    let scale = |v: u64| {
        if max == 0 || v == 0 {
            0
        } else {
            ((v as f64 / max as f64 * (width * 8) as f64).round() as usize).max(1)
        }
    };
    let total = scale(value);
    // keep at least an eighth of the non-thinking output visible
    let thinking = if thinking < value { scale(thinking).min(total.saturating_sub(1)) } else { total };
    let shade = Category::thinking_color();
    // without color, thinking is told apart by texture
    let (solid, textured) = if style.color { ('█', '█') } else { ('█', '▓') };

    let mut s = String::new();
    let mut current = None;
    for cell in 0..width {
        let fill = |eighths: usize| eighths.saturating_sub(cell * 8).min(8);
        let (ch, fg, bg) = match (fill(thinking), fill(total)) {
            (8, _) => (textured, shade, gruvbox::BG1),
            (0, 8) => (solid, color, gruvbox::BG1),
            (0, t) => (EIGHTHS[t], color, gruvbox::BG1),
            // thinking ends mid-cell, with the rest of the output filling the cell behind it
            (k, 8) if !style.color => (if k >= 4 { textured } else { solid }, shade, color),
            (k, 8) => (EIGHTHS[k], shade, color),
            // both segments end in this cell: the larger share takes it
            (k, t) => (EIGHTHS[t], if 2 * k >= t { shade } else { color }, gruvbox::BG1),
        };
        if current != Some((fg, bg)) {
            current = Some((fg, bg));
            s.push_str(&style.bg(bg));
            s.push_str(&style.fg(fg));
        }
        s.push(ch);
    }
    s.push_str(style.reset());
    if !style.color {
        // no track background without color; keep the column edge visible
        s = s.replace(' ', "·");
    }
    s
}

fn context_cell(row: &Row, width: usize, style: &Style) -> String {
    let peak = human(row.peak_context);
    let Some(window) = row.window else {
        return style.paint(gruvbox::FG4, &format!("{peak:>width$}"));
    };

    let ratio = row.peak_context as f64 / window.max(1) as f64;
    let emphasis = if ratio >= 0.8 { style.bold() } else { "" };
    let window = human(window);
    let pad = width.saturating_sub(peak.len() + 1 + window.len());
    format!(
        "{}{emphasis}{}{}",
        " ".repeat(pad),
        style.paint(utilization_color(ratio), &peak),
        style.paint(gruvbox::GRAY, &format!("/{window}"))
    )
}

/// `rust/sift` with the disambiguating parent directories dimmer than the project directory.
fn project_cell(name: &str, width: usize, (parent, dir): (Rgb, Rgb), style: &Style) -> String {
    let pad = " ".repeat(width.saturating_sub(name.chars().count()));
    match name.rsplit_once('/') {
        Some((path, base)) => format!("{}{}{pad}", style.paint(parent, &format!("{path}/")), style.paint(dir, base)),
        None => format!("{}{pad}", style.paint(dir, name)),
    }
}

/// `$12.34 ` or `$12.34?` when some calls couldn't be priced (so the figure is a lower bound).
fn cost_cell(cost: f64, partial: bool, width: usize, style: &Style) -> String {
    let w = width - 1;
    let mark = if partial { style.paint(gruvbox::RED, "?") } else { " ".to_string() };
    format!("{}{mark}", style.paint(gruvbox::GREEN, &format!("{:>w$}", usd(cost))))
}

fn utilization_color(ratio: f64) -> Rgb {
    let ratio = ratio.clamp(0.0, 1.0);
    UTILIZATION
        .windows(2)
        .find(|w| ratio <= w[1].0)
        .map(|w| {
            let ((lo, a), (hi, b)) = (w[0], w[1]);
            a.lerp(b, (ratio - lo) / (hi - lo))
        })
        .unwrap_or(gruvbox::RED)
}

/// `── Sun 2026-09-27 ─────…` spanning the table.
fn day_separator(date: &str, width: usize, style: &Style) -> String {
    let lead = "── ";
    let tail = width.saturating_sub(lead.chars().count() + date.len() + 1);
    format!(
        "{}{}{}\n",
        style.paint(gruvbox::BG1.lerp(gruvbox::GRAY, 0.5), lead),
        style.paint(gruvbox::FG4, date),
        style.paint(gruvbox::BG1.lerp(gruvbox::GRAY, 0.5), &format!(" {}", "─".repeat(tail)))
    )
}

/// `cache write` -> `cache w…`
fn truncate(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        s.to_string()
    } else if width == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(width - 1).collect();
        t.push('…');
        t
    }
}

/// `home/u/rust/sip` -> `…ust/sip`: project names are told apart by their end.
fn truncate_start(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        s.to_string()
    } else if width == 0 {
        String::new()
    } else {
        let mut t = String::from('…');
        t.extend(s.chars().skip(n - (width - 1)));
        t
    }
}

/// Every line of `text` cut to `width` columns, a cut line ending in `…`. The last resort
/// for terminals too narrow for any layout; escape sequences take no columns.
pub fn fit(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        clip(body, width, &mut out);
        out.push_str(newline);
    }
    out
}

fn clip(line: &str, width: usize, out: &mut String) {
    if visible_width(line) <= width {
        out.push_str(line);
        return;
    }
    let mut col = 0;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            out.push(ch);
            for c in chars.by_ref() {
                out.push(c);
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if col + 1 < width {
            out.push(ch);
            col += 1;
        } else {
            if width > 0 {
                out.push('…');
            }
            break;
        }
    }
    if line.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
}

/// Columns taken by `s`, not counting escape sequences (`ESC [ … letter`).
fn visible_width(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            chars.by_ref().find(|c| c.is_ascii_alphabetic());
        } else {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, project: &str, scale: u64) -> Row {
        Row {
            label: label.to_string(),
            project: project.to_string(),
            start: "2026-10-31T12:00:00Z".parse().unwrap(),
            tokens: Tokens::new(300 * scale, 90_000 * scale, 250_000 * scale, 15_000_000 * scale, 40_000 * scale),
            turns: 120,
            cost: 6.8 * scale as f64,
            unpriced: 0,
            models: vec![("claude-opus-5-5".to_string(), 100), ("claude-haiku-4-5".to_string(), 20)],
            peak_context: 287_000,
            window: Some(1_000_000),
        }
    }

    #[test]
    fn lines_never_exceed_the_terminal() {
        let rows = [
            row("10-31 12:00", "home/u/some-rather-long-project-name", 1),
            row("10-31 14:30", "rust/sip", 3),
        ];
        for color in [false, true] {
            for width in 20..=220 {
                let style = Style { color, width };
                for by in [GroupBy::Session, GroupBy::Project] {
                    let out = render(&rows, &Category::ALL, by, &style);
                    for line in out.lines() {
                        assert!(visible_width(line) <= width, "{width} cols, {by:?}: {line:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn project_names_give_way_before_categories() {
        let rows = [row("10-31 12:00", "home/u/some-rather-long-project-name", 1)];
        let out = render(&rows, &Category::ALL, GroupBy::Session, &Style { color: false, width: 100 });
        assert!(out.contains("cache read"), "{out}");
        assert!(out.contains("…ct-name"), "{out}");
    }

    #[test]
    fn truncation_keeps_the_meaningful_end() {
        assert_eq!(truncate("cache write", 8), "cache w…");
        assert_eq!(truncate_start("home/u/rust/sip", 8), "…ust/sip");
        assert_eq!(truncate_start("sip", 8), "sip");
        assert_eq!(truncate_start("sip", 0), "");
    }

    #[test]
    fn fit_clips_by_visible_columns() {
        let painted = "\x1b[38;2;1;2;3mabcdef\x1b[0m\nok\n";
        assert_eq!(fit(painted, 6), painted);
        assert_eq!(fit(painted, 4), "\x1b[38;2;1;2;3mabc…\x1b[0m\nok\n");
        assert_eq!(fit("abcdef", 4), "abc…");
    }
}
