// Model-output: Claude Opus 5.5
// Model-output: Claude Fable 5.1

//! Formatting quantities and tables for people.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use std::collections::BTreeMap;

/// The units above bytes, each 1000 times the last.
const UNITS: &[&str] = &["kB", "MB", "GB", "TB", "PB", "EB"];

/// Formats `bytes` with two decimals in the first of [`UNITS`] that keeps
/// the number under 1000 (or else the last), e.g. "0.52 kB" or "130.50 GB".
fn kilobytes_or_more(bytes: f64) -> String {
    let mut value = bytes / 1000.0;
    let mut unit = 0;
    // What would round up to "1000.00" goes to the next unit.
    while value >= 999.995 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// Formats a byte count with SI units, e.g. "999 B" or "130.50 GB".
pub fn bytes(bytes: u64) -> String {
    if bytes < 1000 { format!("{bytes} B") } else { kilobytes_or_more(bytes as f64) }
}

/// Formats a rate in bytes per second, in kB/s or more so that every rate
/// has two decimals, e.g. "0.52 kB/s" or "1.39 GB/s".
pub fn rate(bytes_per_sec: f64) -> String {
    assert!(bytes_per_sec.is_finite() && bytes_per_sec >= 0.0, "not a rate: {bytes_per_sec}");
    format!("{}/s", kilobytes_or_more(bytes_per_sec))
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

/// Formats counters by name, e.g. "csum_errors=3 read_errs=1".
pub fn counters(counters: &BTreeMap<String, u64>) -> String {
    let counts: Vec<_> = counters.iter().map(|(name, count)| format!("{name}={count}")).collect();
    counts.join(" ")
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
    Dim,
    Red,
    Green,
}

impl Style {
    /// A table cell showing `text` in this style, flush left.
    pub fn cell(self, text: impl Into<String>) -> Cell {
        Cell { spans: vec![(self, text.into())], align: Align::Left }
    }

    /// `text` in this style, with ANSI escape sequences.
    pub fn paint(self, text: &str) -> String {
        let code = match self {
            Style::Plain => return text.to_string(),
            Style::Bold  => "1",
            Style::Dim   => "2",
            Style::Red   => "31",
            Style::Green => "32",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

/// Where a cell's text goes in its column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    /// Any odd space left over goes on the right.
    Center,
}

/// Text in a table, and how it looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The text, in pieces that each have a style.
    pub spans: Vec<(Style, String)>,
    pub align: Align,
}

impl Cell {
    /// This cell, aligned as `align` says.
    pub fn aligned(self, align: Align) -> Cell {
        Cell { align, ..self }
    }

    /// This cell with `text` in `style` added at the end.
    pub fn then(mut self, style: Style, text: impl Into<String>) -> Cell {
        self.spans.push((style, text.into()));
        self
    }

    /// How many characters the text has.
    fn chars(&self) -> usize {
        self.spans.iter().map(|(_, text)| text.chars().count()).sum()
    }
}

/// A cell showing `time` in `zone` to the minute, without the year, like
/// "12-31T23:59", the "T" dim to set apart the date and the time of day.
pub fn minute(time: Timestamp, zone: &TimeZone) -> Cell {
    let zoned = time.to_zoned(zone.clone());
    let (date, time_of_day) = (zoned.strftime("%m-%d").to_string(), zoned.strftime("%H:%M").to_string());
    Style::Plain.cell(date).then(Style::Dim, "T").then(Style::Plain, time_of_day)
}

/// Lays out `rows` of cells in columns, two spaces apart, without trailing
/// spaces, each cell aligned its own way, and in the cells' styles if
/// `color`.  All rows must have the same number of cells.
pub fn table(rows: &[Vec<Cell>], color: bool) -> String {
    let columns = rows.first().map_or(0, Vec::len);
    assert!(rows.iter().all(|row| row.len() == columns), "rows have different numbers of cells");
    let widths: Vec<usize> = (0..columns).map(|i| rows.iter().map(|row| row[i].chars()).max().unwrap_or(0)).collect();
    let mut text = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, &width)| {
                // Padding counts chars, like the widths, and goes outside any
                // escape sequences (none around empty text) so that
                // trim_end can remove it from the end of a row.
                let paint = |(style, text): &(Style, String)| if color && !text.is_empty() { style.paint(text) } else { text.clone() };
                let shown: String = cell.spans.iter().map(paint).collect();
                let padding = width - cell.chars();
                let before = match cell.align {
                    Align::Left   => 0,
                    Align::Right  => padding,
                    Align::Center => padding / 2,
                };
                " ".repeat(before) + &shown + &" ".repeat(padding - before)
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
    use jiff::tz;

    #[test]
    fn formats() {
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_000), "1.00 kB");
        assert_eq!(bytes(391_560_000_000), "391.56 GB");
        assert_eq!(bytes(999_994), "999.99 kB");
        assert_eq!(bytes(999_999), "1.00 MB");
        assert_eq!(rate(1_389_999.6), "1.39 MB/s");
        assert_eq!(bytes(u64::MAX), "18.45 EB");
        assert_eq!(rate(524.0), "0.52 kB/s");
        assert_eq!(rate(0.0), "0.00 kB/s");
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
        let cell = |text: &str, align| Style::Plain.cell(text).aligned(align);
        let rows = [
            vec![cell("net", Align::Center), cell("load", Align::Left), cell("x", Align::Left)],
            vec![cell("1.5 kB/s", Align::Right), cell("0.5", Align::Right), cell("", Align::Right)],
        ];
        assert_eq!(table(&rows, false), "  net     load  x\n1.5 kB/s   0.5\n");
    }

    #[test]
    fn styles_tables() {
        let rows = [vec![Style::Bold.cell("NAME"), Style::Bold.cell("OKAY")], vec![Style::Plain.cell("three"), Style::Red.cell("no")]];
        assert_eq!(table(&rows, true), "\x1b[1mNAME\x1b[0m   \x1b[1mOKAY\x1b[0m\nthree  \x1b[31mno\x1b[0m\n");
        assert_eq!(table(&rows, false), "NAME   OKAY\nthree  no\n");
        assert_eq!(table(&[vec![Style::Plain.cell("a"), Style::Red.cell("")]], true), "a\n");
        let rows = [vec![Style::Bold.cell("NET").aligned(Align::Center)], vec![Style::Red.cell("12.50").aligned(Align::Right)], vec![Style::Red.cell("1.0").aligned(Align::Right)]];
        assert_eq!(table(&rows, true), " \x1b[1mNET\x1b[0m\n\x1b[31m12.50\x1b[0m\n  \x1b[31m1.0\x1b[0m\n");
        let rows = [vec![Style::Plain.cell("ab").then(Style::Red, "c").then(Style::Green, ""), Style::Plain.cell("x")], vec![Style::Plain.cell("a"), Style::Plain.cell("y")]];
        assert_eq!(table(&rows, true), "ab\x1b[31mc\x1b[0m  x\na    y\n");
    }

    #[test]
    fn shows_minutes() {
        let time: Timestamp = "2026-12-31T22:30:59Z".parse().unwrap();
        let cell = minute(time, &TimeZone::fixed(tz::offset(2)));
        assert_eq!(cell.spans, [(Style::Plain, "01-01".into()), (Style::Dim, "T".into()), (Style::Plain, "00:30".into())]);
        assert_eq!(minute(time, &TimeZone::UTC).spans[2].1, "22:30");
    }
}
