//! Terminal renderer: one row per group, one fractional-block bar per category.
//! Each category column is scaled to its own maximum, so bars compare rows, not categories.

use std::fmt::Write;

use jiff::tz::TimeZone;

use crate::aggregate::{GroupBy, Row};
use crate::model::{Category, Rgb, Tokens, gruvbox, human};
use crate::pricing::usd;

const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
const NUM_WIDTH: usize = 6;

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
    fn fg(&self, c: Rgb) -> String {
        if self.color { format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2) } else { String::new() }
    }

    fn bg(&self, c: Rgb) -> String {
        if self.color { format!("\x1b[48;2;{};{};{}m", c.0, c.1, c.2) } else { String::new() }
    }

    fn bold(&self) -> &'static str {
        if self.color { "\x1b[1m" } else { "" }
    }

    fn reset(&self) -> &'static str {
        if self.color { "\x1b[0m" } else { "" }
    }

    fn paint(&self, c: Rgb, s: &str) -> String {
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

    let mut fixed = label_w + 2 + model_w + 2 + ctx_w + 2 + cost_w + 2;
    if project_w > 0 {
        fixed += project_w + 2;
    }
    let per_cat = style.width.saturating_sub(fixed) / categories.len().max(1);
    // bar + space + number + gap
    let bar_w = per_cat.saturating_sub(NUM_WIDTH + 3).clamp(4, 40);

    let max: Vec<u64> = categories
        .iter()
        .map(|&c| rows.iter().map(|r| r.tokens.get(c)).max().unwrap_or(0))
        .collect();

    let table_w = fixed + categories.len() * (bar_w + 1 + NUM_WIDTH + 2) - 2;
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
    let cell_w = bar_w + 1 + NUM_WIDTH;
    for &c in categories {
        let label = c.label();
        // the output bar leads with its thinking share; name the dimmer shade when there's room
        let legend = " thinking";
        if c == Category::Output && label.len() + legend.len() <= cell_w {
            out.push_str(&style.paint(c.color(), label));
            out.push_str(&style.paint(Category::thinking_color(), &format!("{legend:<w$}", w = cell_w - label.len())));
        } else {
            out.push_str(&style.paint(c.color(), &format!("{label:<cell_w$}")));
        }
        out.push_str("  ");
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

        out.push_str(&style.paint(gruvbox::FG, &format!("{:label_w$}  ", row.label)));
        if project_w > 0 {
            out.push_str(&style.paint(gruvbox::PURPLE, &format!("{:project_w$}  ", truncate(&row.project, project_w))));
        }
        out.push_str(&style.paint(gruvbox::FG4, &format!("{:model_w$}  ", row.model_label())));
        out.push_str(&context_cell(row, ctx_w, style));
        out.push_str("  ");
        out.push_str(&cost_cell(row.cost, row.unpriced > 0, cost_w, style));
        out.push_str("  ");

        for (&c, &m) in categories.iter().zip(&max) {
            let v = row.tokens.get(c);
            let thinking = if c == Category::Output { row.tokens.thinking() } else { 0 };
            out.push_str(&bar(v, thinking, m, bar_w, c.color(), style));
            out.push(' ');
            out.push_str(&style.paint(gruvbox::FG, &format!("{:>NUM_WIDTH$}", human(v))));
            out.push_str("  ");
        }
        out.push('\n');
    }

    // totals
    let mut total = Tokens::default();
    rows.iter().for_each(|r| total += r.tokens);
    let turns: usize = rows.iter().map(|r| r.turns).sum();
    let summary = format!("{} rows, {} calls", rows.len(), turns);
    let pad = fixed.saturating_sub(cost_w + 4);
    out.push_str(&style.paint(gruvbox::GRAY, &format!("{summary:>pad$}  ")));
    let unpriced = rows.iter().any(|r| r.unpriced > 0);
    out.push_str(&cost_cell(total_cost, unpriced, cost_w, style));
    out.push_str("  ");
    for &c in categories {
        let sum = human(total.get(c));
        let thinking = human(total.thinking());
        if c == Category::Output && total.thinking() > 0 && thinking.len() + 1 + sum.len() <= cell_w {
            let w = cell_w - sum.len() - 1;
            out.push_str(&style.paint(Category::thinking_color(), &format!("{thinking:>w$} ")));
            out.push_str(&style.paint(c.color(), &sum));
        } else {
            out.push_str(&style.paint(c.color(), &format!("{sum:>cell_w$}")));
        }
        out.push_str("  ");
    }
    out.push('\n');
    out
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

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(width - 1).collect();
        t.push('…');
        t
    }
}
