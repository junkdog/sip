//! Calendar renderer: `cal`-style month grids, each day shaded by its cost or its tokens of a
//! single category. Shades are buckets bounded by quantiles of the active days, so skewed usage
//! still spreads out.

use std::collections::HashMap;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

use crate::aggregate::{Row, week_start};
use crate::ansi::Style;
use crate::model::{Category, Rgb, gruvbox, human};

/// What the shades measure.
#[derive(Clone, Copy)]
pub enum Metric {
    Cost,
    Tokens(Category),
}

impl Metric {
    /// Whole tokens, or cost in cents so both bucket as integers.
    fn value(self, row: &Row) -> u64 {
        match self {
            Metric::Cost => (row.cost * 100.0).round() as u64,
            Metric::Tokens(c) => row.tokens.get(c),
        }
    }

    fn format(self, v: u64) -> String {
        match self {
            Metric::Cost => dollars(v),
            Metric::Tokens(_) => human(v),
        }
    }

    fn label(self) -> String {
        match self {
            Metric::Cost => "cost/day".to_string(),
            Metric::Tokens(c) => format!("{} tokens/day", c.label()),
        }
    }

    fn color(self) -> Rgb {
        match self {
            Metric::Cost => gruvbox::GREEN,
            Metric::Tokens(c) => c.color(),
        }
    }
}

/// Number of shades; fewer when the active days don't spread over that many distinct bounds.
const LEVELS: usize = 5;
/// Without color, a glyph after the day number stands in for its shade, lightest first.
const GLYPHS: [char; LEVELS] = ['·', '░', '▒', '▓', '█'];
/// Week number plus seven days, each cell followed by a column for its glyph.
const MONTH_W: usize = 3 + 7 * 3;
const GAP: usize = 2;
/// Day numbers outside the date range.
const OUTSIDE: Rgb = Rgb(0x66, 0x5c, 0x54);

/// Every month touching `first..=last` in full, laid out as many per line as `style.width` fits.
pub fn render(rows: &[Row], metric: Metric, (first, last): (Date, Date), style: &Style) -> String {
    let tz = TimeZone::system();
    let mut usage: HashMap<Date, u64> = HashMap::new();
    for row in rows {
        *usage.entry(row.start.to_zoned(tz.clone()).date()).or_default() += metric.value(row);
    }
    usage.retain(|d, v| (first..=last).contains(d) && *v > 0);

    let cal = Calendar {
        usage: &usage,
        range: (first, last),
        today: Timestamp::now().to_zoned(tz).date(),
        scale: Scale::new(usage.values().copied().collect()),
        metric,
        color: metric.color(),
        style,
    };

    let months: Vec<Date> = first
        .first_of_month()
        .series(1.month())
        .take_while(|m| *m <= last)
        .collect();
    let per_line = ((style.width + GAP) / (MONTH_W + GAP)).max(1);
    let blank = " ".repeat(MONTH_W);

    let mut out = String::new();
    for band in months.chunks(per_line) {
        let grids: Vec<Vec<String>> = band.iter().map(|&m| cal.month(m)).collect();
        let height = grids.iter().map(Vec::len).max().unwrap_or(0);
        for i in 0..height {
            let line: Vec<&str> = grids.iter().map(|g| g.get(i).unwrap_or(&blank).as_str()).collect();
            out.push_str(line.join(&" ".repeat(GAP)).trim_end());
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(&cal.legend());
    out.push_str(&cal.summary());
    out
}

struct Calendar<'a> {
    usage: &'a HashMap<Date, u64>,
    range: (Date, Date),
    today: Date,
    scale: Scale,
    metric: Metric,
    color: Rgb,
    style: &'a Style,
}

impl Calendar<'_> {
    /// Title, weekday header and one line per week, each `MONTH_W` columns wide.
    fn month(&self, first: Date) -> Vec<String> {
        let style = self.style;
        let last = first.last_of_month();
        let total: u64 = first.series(1.day()).take_while(|d| *d <= last).filter_map(|d| self.usage.get(&d)).sum();

        let name = first.strftime("%B %Y").to_string();
        let total = if total > 0 { format!(" · {}", self.metric.format(total)) } else { String::new() };
        // centered over the grid, not counting the last glyph column
        let w = name.chars().count() + total.chars().count();
        let left = (MONTH_W - 1).saturating_sub(w) / 2;
        let title = format!(
            "{}{}{}{}{}",
            " ".repeat(left),
            style.bold(),
            style.paint(gruvbox::FG, &name),
            style.paint(self.color, &total),
            " ".repeat(MONTH_W.saturating_sub(left + w)),
        );

        let mut lines = vec![title, style.paint(gruvbox::FG4, "   Mo Tu We Th Fr Sa Su ")];
        for monday in week_start(first).series(1.week()).take_while(|m| *m <= last) {
            let week = monday.iso_week_date().week();
            let mut line = style.paint(gruvbox::GRAY, &format!("{week:>2} "));
            for date in monday.series(1.day()).take(7) {
                if date.month() == first.month() {
                    line.push_str(&self.day(date));
                } else {
                    line.push_str("   ");
                }
            }
            lines.push(line);
        }
        lines
    }

    /// `dd` on its shade, followed by the glyph column.
    fn day(&self, date: Date) -> String {
        let style = self.style;
        let (first, last) = self.range;
        let level = self.usage.get(&date).map(|&v| self.scale.level(v));

        let mut s = String::new();
        if date == self.today {
            s.push_str(style.bold());
            s.push_str(style.underline());
        }
        let number = format!("{:>2}", date.day());
        match level {
            _ if date < first || date > last => s.push_str(&style.paint(OUTSIDE, &number)),
            None => s.push_str(&style.paint(gruvbox::FG4, &number)),
            Some(i) => {
                let bg = self.scale.shade(i, self.color);
                s.push_str(&style.bg(bg));
                s.push_str(&style.paint(text_on(bg), &number));
            }
        }
        match level {
            Some(i) if !style.color => s.push(self.scale.glyph(i)),
            _ => s.push(' '),
        }
        s
    }

    /// `cost/day  ██ <$1.2  ██ $1.2–$4.5  …  ██ $41+`, wrapped to the terminal width.
    fn legend(&self) -> String {
        let style = self.style;
        let head = self.metric.label();
        let mut entries = vec![(head.chars().count(), style.paint(self.color, &head))];
        for i in 0..self.scale.levels() {
            let label = self.scale.label(i, |v| self.metric.format(v));
            let swatch = if style.color {
                format!("{}  {}", style.bg(self.scale.shade(i, self.color)), style.reset())
            } else {
                self.scale.glyph(i).to_string().repeat(2)
            };
            entries.push((3 + label.chars().count(), format!("{swatch} {}", style.paint(gruvbox::FG4, &label))));
        }
        if self.scale.levels() == 0 {
            entries.push((8, style.paint(gruvbox::GRAY, "no usage")));
        }

        let mut out = String::new();
        let mut col = 0;
        for (w, entry) in entries {
            if col > 0 && col + 2 + w > self.style.width {
                out.push('\n');
                col = 0;
            }
            if col > 0 {
                out.push_str("  ");
                col += 2;
            }
            out.push_str(&entry);
            col += w;
        }
        out.push('\n');
        out
    }

    /// `12 of 30 days active, $420 total, peak $110 on Tue 2026-10-14`
    fn summary(&self) -> String {
        let style = self.style;
        let fmt = |v| self.metric.format(v);
        let (first, last) = self.range;
        let days = first.series(1.day()).take_while(|d| *d <= last).count();
        let total: u64 = self.usage.values().sum();
        let mut s = format!("{} of {days} days active, {} total", self.usage.len(), fmt(total));
        if let Some((date, &peak)) = self.usage.iter().max_by_key(|&(d, v)| (*v, std::cmp::Reverse(*d))) {
            s.push_str(&format!(", peak {} on {}", fmt(peak), date.strftime("%a %Y-%m-%d")));
        }
        style.paint(gruvbox::GRAY, &s) + "\n"
    }
}

/// Buckets of daily usage: level `i` holds the days from `bounds[i - 1]` up to `bounds[i]`.
struct Scale {
    bounds: Vec<u64>,
    /// Smallest and largest active day; `None` without any, leaving no levels at all.
    span: Option<(u64, u64)>,
}

impl Scale {
    fn new(mut values: Vec<u64>) -> Self {
        values.retain(|&v| v > 0);
        values.sort_unstable();
        let (Some(&min), Some(&max)) = (values.first(), values.last()) else {
            return Self { bounds: Vec::new(), span: None };
        };
        let mut bounds: Vec<u64> = (1..LEVELS).map(|k| round_sig(values[k * values.len() / LEVELS])).collect();
        bounds.dedup();
        // rounding may push a bound past either end, leaving its outer level empty
        bounds.retain(|&b| b > min && b <= max);
        Self { bounds, span: Some((min, max)) }
    }

    fn levels(&self) -> usize {
        if self.span.is_some() { self.bounds.len() + 1 } else { 0 }
    }

    fn level(&self, value: u64) -> usize {
        self.bounds.iter().filter(|&&b| value >= b).count()
    }

    /// From a dim tint of `color` at the lowest level to `color` itself at the highest.
    fn shade(&self, level: usize, color: Rgb) -> Rgb {
        let dim = gruvbox::BG1.lerp(color, 0.25);
        match self.levels() {
            0 | 1 => color,
            n => dim.lerp(color, level as f64 / (n - 1) as f64),
        }
    }

    /// Spread over the glyphs the same way the shades spread over the gradient.
    fn glyph(&self, level: usize) -> char {
        match self.levels() {
            0 | 1 => GLYPHS[LEVELS - 1],
            n => GLYPHS[level * (LEVELS - 1) / (n - 1)],
        }
    }

    fn label(&self, level: usize, fmt: impl Fn(u64) -> String) -> String {
        let lo = level.checked_sub(1).map(|i| self.bounds[i]);
        match (lo, self.bounds.get(level)) {
            (None, Some(&hi)) => format!("<{}", fmt(hi)),
            (Some(lo), Some(&hi)) => format!("{}–{}", fmt(lo), fmt(hi)),
            (Some(lo), None) => format!("{}+", fmt(lo)),
            (None, None) => match self.span {
                Some((min, max)) if fmt(min) != fmt(max) => format!("{}–{}", fmt(min), fmt(max)),
                Some((min, _)) => fmt(min),
                None => String::new(),
            },
        }
    }
}

/// `n` rounded to two significant digits, so bucket bounds read as round figures.
fn round_sig(n: u64) -> u64 {
    let mut scale = 1;
    while n / scale >= 100 {
        scale *= 10;
    }
    (n + scale / 2) / scale * scale
}

/// Cents as a short dollar figure: `$0.45`, `$4.5`, `$120`, `$1.3k`.
fn dollars(cents: u64) -> String {
    let usd = cents as f64 / 100.0;
    match cents {
        0..100 => format!("${usd:.2}"),
        100..1_000 => format!("${usd:.1}"),
        1_000..100_000 => format!("${usd:.0}"),
        100_000..1_000_000 => format!("${:.1}k", usd / 1_000.0),
        _ => format!("${:.0}k", usd / 1_000.0),
    }
}

/// Dark text on bright shades, light text on dim ones.
fn text_on(bg: Rgb) -> Rgb {
    let luma = 0.2126 * bg.0 as f64 + 0.7152 * bg.1 as f64 + 0.0722 * bg.2 as f64;
    if luma > 110.0 { gruvbox::BG } else { gruvbox::FG }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_sig_keeps_two_digits() {
        assert_eq!(round_sig(7), 7);
        assert_eq!(round_sig(99), 99);
        assert_eq!(round_sig(1_234), 1_200);
        assert_eq!(round_sig(125_000), 130_000);
        assert_eq!(round_sig(99_500), 100_000);
    }

    #[test]
    fn dollars_stay_short() {
        assert_eq!(dollars(5), "$0.05");
        assert_eq!(dollars(450), "$4.5");
        assert_eq!(dollars(12_000), "$120");
        assert_eq!(dollars(130_000), "$1.3k");
        assert_eq!(dollars(4_200_000), "$42k");
    }

    #[test]
    fn scale_buckets_by_quantile() {
        let scale = Scale::new((1..=100).map(|v| v * 1_000).collect());
        assert_eq!(scale.bounds, [21_000, 41_000, 61_000, 81_000]);
        assert_eq!(scale.levels(), LEVELS);
        assert_eq!(scale.level(1_000), 0);
        assert_eq!(scale.level(21_000), 1);
        assert_eq!(scale.level(100_000), 4);
        assert_eq!(scale.label(0, human), "<21k");
        assert_eq!(scale.label(1, human), "21k–41k");
        assert_eq!(scale.label(4, human), "81k+");
    }

    #[test]
    fn scale_collapses_without_spread() {
        let scale = Scale::new(vec![0, 5_000, 5_000, 5_000]);
        assert!(scale.bounds.is_empty());
        assert_eq!(scale.levels(), 1);
        assert_eq!(scale.label(0, human), "5.0k");
        assert_eq!(scale.shade(0, gruvbox::ORANGE), gruvbox::ORANGE);

        let scale = Scale::new(vec![0, 0]);
        assert_eq!(scale.levels(), 0);
    }
}
