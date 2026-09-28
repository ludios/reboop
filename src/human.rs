// Model-output: Claude Opus 5.5

//! Formatting quantities and tables for people.

use std::env;
use std::io::{self, IsTerminal};

/// Formats a byte count with SI units, e.g. "130.50GB".
pub fn bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in UNITS {
        // What would round up to "1000.00" goes to the next unit.
        if value < 999.995 {
            break;
        }
        value /= 1000.0;
        unit = next;
    }
    format!("{value:.2}{unit}")
}

/// Formats a rate in bytes per second, e.g. "1.39GB/s".
pub fn rate(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec.round() as u64))
}

/// Formats seconds like "1h 2m 3s", leaving out leading zero units.
pub fn seconds(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m {seconds}s"),
        _ => format!("{hours}h {minutes}m {seconds}s"),
    }
}

/// `text`, cut to at most `max_chars` characters with an ellipsis at the end
/// if it's longer.
pub fn truncate(text: &str, max_chars: usize) -> String {
    assert!(max_chars > 0, "no room for even an ellipsis");
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars - 1).collect();
    cut.push('…');
    cut
}

/// How text looks on a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Bold,
    Red,
    Green,
    Gray,
}

impl Style {
    /// A table cell showing `text` in this style.
    pub fn cell(self, text: impl Into<String>) -> Cell {
        Cell { text: text.into(), style: self }
    }

    /// `text` in this style, with ANSI escape sequences.
    fn paint(self, text: &str) -> String {
        let code = match self {
            Style::Plain => return text.to_string(),
            Style::Bold  => "1",
            Style::Red   => "31",
            Style::Green => "32",
            Style::Gray  => "90",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

/// Text in a table, and how it looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub style: Style,
}

/// Whether to style what's printed to stdout: only if it's a terminal and
/// NO_COLOR isn't set (<https://no-color.org>).
pub fn color_stdout() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// Lays out `rows` of cells in columns, two spaces apart, without trailing
/// spaces, and in the cells' styles if `color`.  All rows must have the same
/// number of cells.
pub fn table(rows: &[Vec<Cell>], color: bool) -> String {
    let columns = rows.first().map_or(0, Vec::len);
    assert!(rows.iter().all(|row| row.len() == columns), "rows have different numbers of cells");
    let widths: Vec<usize> = (0..columns).map(|i| rows.iter().map(|row| row[i].text.chars().count()).max().unwrap_or(0)).collect();
    let mut text = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, &width)| {
                // Padding counts chars, like the widths, and goes after any
                // escape sequences so that trim_end can remove it.
                let shown = if color { cell.style.paint(&cell.text) } else { cell.text.clone() };
                shown + &" ".repeat(width - cell.text.chars().count())
            })
            .collect();
        text.push_str(cells.join("  ").trim_end());
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(999), "999B");
        assert_eq!(bytes(1_000), "1.00kB");
        assert_eq!(bytes(391_560_000_000), "391.56GB");
        assert_eq!(bytes(999_994), "999.99kB");
        assert_eq!(bytes(999_999), "1.00MB");
        assert_eq!(rate(1_389_999.6), "1.39MB/s");
        assert_eq!(seconds(5), "5s");
        assert_eq!(seconds(210), "3m 30s");
        assert_eq!(seconds(3723), "1h 2m 3s");
        assert_eq!(truncate("tmux", 4), "tmux");
        assert_eq!(truncate("tmux new", 4), "tmu…");
    }

    #[test]
    fn lays_out_tables() {
        let rows = [vec!["name", "kernel", "okay"], vec!["one", "6.18.54 → 6.18.55", "no"], vec!["three", "", ""]];
        let rows: Vec<Vec<Cell>> = rows.iter().map(|row| row.iter().map(|&text| Style::Plain.cell(text)).collect()).collect();
        assert_eq!(table(&rows, false), "name   kernel             okay\none    6.18.54 → 6.18.55  no\nthree\n");
        assert_eq!(table(&rows, true), table(&rows, false));
        assert_eq!(table(&[], true), "");
    }

    #[test]
    fn styles_tables() {
        let rows = [vec![Style::Bold.cell("NAME"), Style::Bold.cell("OKAY")], vec![Style::Plain.cell("three"), Style::Red.cell("no")]];
        assert_eq!(table(&rows, true), "\x1b[1mNAME\x1b[0m   \x1b[1mOKAY\x1b[0m\nthree  \x1b[31mno\x1b[0m\n");
        assert_eq!(table(&rows, false), "NAME   OKAY\nthree  no\n");
    }
}
