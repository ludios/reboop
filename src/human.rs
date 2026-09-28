// Model-output: Claude Opus 5.5

//! Formatting quantities for people.

/// Formats a byte count with SI units, e.g. "130.50GB".
pub fn bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = next;
    }
    format!("{value:.2}{unit}")
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

/// Lays out `rows` of cells in columns, two spaces apart, without trailing
/// spaces.  All rows must have the same number of cells.
pub fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.first().map_or(0, Vec::len);
    assert!(rows.iter().all(|row| row.len() == columns), "rows have different numbers of cells");
    let widths: Vec<usize> = (0..columns).map(|i| rows.iter().map(|row| row[i].chars().count()).max().unwrap_or(0)).collect();
    let mut text = String::new();
    for row in rows {
        // Padding counts chars, like the widths.
        let cells: Vec<String> = row.iter().zip(&widths).map(|(cell, &width)| format!("{cell:<width$}")).collect();
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
        assert_eq!(seconds(5), "5s");
        assert_eq!(seconds(210), "3m 30s");
        assert_eq!(seconds(3723), "1h 2m 3s");
        assert_eq!(truncate("tmux", 4), "tmux");
        assert_eq!(truncate("tmux new", 4), "tmu…");
    }

    #[test]
    fn lays_out_tables() {
        let rows = [vec!["name", "kernel", "okay"], vec!["one", "6.18.54 → 6.18.55", "NO"], vec!["three", "", ""]];
        let rows: Vec<Vec<String>> = rows.iter().map(|row| row.iter().map(|cell| cell.to_string()).collect()).collect();
        assert_eq!(table(&rows), "name   kernel             okay\none    6.18.54 → 6.18.55  NO\nthree\n");
        assert_eq!(table(&[]), "");
    }
}
