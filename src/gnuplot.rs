//! PNG renderer: one bar chart per category, stacked vertically on a shared x axis.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::aggregate::Row;
use crate::model::{Category, gruvbox};

const PANEL_HEIGHT: usize = 260;
/// Vertical pixels between panels, so adjacent y tick labels don't collide.
const PANEL_GAP: f64 = 28.0;

pub fn render(rows: &[Row], categories: &[Category], output: &Path, title: &str) -> Result<()> {
    let data_path = output.with_extension("dat");
    let script_path = output.with_extension("gnuplot");

    fs::write(&data_path, data(rows))
        .with_context(|| format!("writing {}", data_path.display()))?;
    fs::write(&script_path, script(rows, categories, &data_path, output, title))
        .with_context(|| format!("writing {}", script_path.display()))?;

    let result = Command::new("gnuplot")
        .arg(&script_path)
        .output()
        .context("failed to run gnuplot; is it installed?")?;
    if !result.status.success() {
        bail!("gnuplot failed:\n{}", String::from_utf8_lossy(&result.stderr));
    }
    Ok(())
}

/// Shows the PNG inline when running inside kitty; returns false otherwise.
pub fn display_inline(png: &Path) -> Result<bool> {
    let in_kitty = std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM").is_ok_and(|t| t == "xterm-kitty");
    if !in_kitty || !std::io::stdout().is_terminal() {
        return Ok(false);
    }
    let status = Command::new("kitty")
        .args(["+kitten", "icat", "--align", "left"])
        .arg(png)
        .status()
        .context("failed to run `kitty +kitten icat`")?;
    Ok(status.success())
}

pub fn default_output() -> PathBuf {
    std::env::temp_dir().join(format!("sip-{}", std::process::id())).with_extension("png")
}

fn data(rows: &[Row]) -> String {
    let mut out = String::from("# idx label input output cache_write cache_read\n");
    for (i, row) in rows.iter().enumerate() {
        let label = match row.project.as_str() {
            "" => row.label.clone(),
            p => format!("{} {p}", row.label),
        };
        let values: Vec<String> = Category::ALL.iter().map(|&c| row.tokens.get(c).to_string()).collect();
        out.push_str(&format!("{i} \"{}\" {}\n", label.replace('"', "'"), values.join(" ")));
    }
    out
}

fn script(rows: &[Row], categories: &[Category], data: &Path, output: &Path, title: &str) -> String {
    let n = categories.len();
    let width = (rows.len() * 28 + 260).clamp(1000, 3200);
    let height = n * PANEL_HEIGHT + 220;
    // leave room below the last panel for rotated tick labels
    let bottom_margin = 0.14 * 900.0 / height as f64;
    let top_margin = 60.0 / height as f64;
    let panel = (1.0 - bottom_margin - top_margin) / n as f64;

    let hex = |c: crate::model::Rgb| c.hex();
    let mut s = format!(
        r#"set terminal pngcairo size {width},{height} enhanced font 'monospace,11' background '{bg}'
set output '{output}'
set border 3 lc rgb '{fg4}' lw 1.2
set grid ytics lc rgb '{bg1}' lw 1
set tics nomirror textcolor rgb '{fg}'
set ylabel textcolor rgb '{fg4}'
set key off
set style fill solid 0.9 noborder
set boxwidth 0.75
set format y '%.1s%c'
set xrange [-0.6:{xmax}]
set yrange [0:*]
set lmargin 12
set rmargin 4
set multiplot title "{{/:Bold {title}}}" font ',15' textcolor rgb '{yellow}'
"#,
        bg = hex(gruvbox::BG),
        fg = hex(gruvbox::FG),
        fg4 = hex(gruvbox::FG4),
        bg1 = hex(gruvbox::BG1),
        yellow = hex(gruvbox::YELLOW),
        output = output.display(),
        xmax = rows.len() as f64 - 0.4,
        title = title.replace('"', "'"),
    );

    for (i, &c) in categories.iter().enumerate() {
        let last = i + 1 == n;
        let top = 1.0 - top_margin - panel * i as f64;
        let bottom = top - panel;
        s.push_str(&format!(
            "set tmargin at screen {top:.4}\nset bmargin at screen {:.4}\n",
            bottom + PANEL_GAP / height as f64
        ));
        s.push_str(&format!("set ylabel '{}' textcolor rgb '{}'\n", c.label(), c.color().hex()));
        let xtic = if last {
            s.push_str("set xtics rotate by 45 right font ',9'\n");
            ":xtic(2)"
        } else {
            s.push_str("set xtics format ''\n");
            ""
        };
        s.push_str(&format!(
            "plot '{}' using 1:{}{xtic} with boxes lc rgb '{}'\n",
            data.display(),
            c as usize + 3,
            c.color().hex()
        ));
    }
    s.push_str("unset multiplot\n");
    s
}
