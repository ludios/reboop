//! Telling the user how a command is going, with lines (which can be
//! timestamped and styled) and, on a terminal, with a line of progress that
//! is rewritten in place, or with lines that are drawn over each time they
//! change.

use crate::human::{self, Style::{self, Bold, Dim, Plain, Red}};
use jiff::Zoned;
use std::fmt;
use std::io::{self, IsTerminal, Write};

/// Tells the user how a command is going.  Write errors are ignored: a
/// command whose stdout is closed should still finish what it started.
pub struct Printer<'a> {
    out: &'a mut dyn Write,
    /// Whether to style the output: for example, problems in red, values in
    /// bold, and timestamps dimmed.
    pub(crate) color: bool,
    /// If progress is shown by rewriting the last line (which takes a
    /// terminal), gives the terminal's width, which can change.
    columns: Option<fn() -> usize>,
    /// If lines start with a timestamp, gives the time of day, like
    /// "12:03:16".
    clock: Option<fn() -> String>,
    /// How many characters of progress are on the last line.
    progress_chars: usize,
}

impl<'a> Printer<'a> {
    /// A printer that writes to `out`, and:
    ///
    /// - styles the output if `color`;
    /// - shows progress only if `columns` is given, by rewriting the last
    ///   line, cut to fit in one row of the terminal; `columns` returns the
    ///   terminal's width, and is called for each update of progress, since
    ///   the width can change;
    /// - starts each line with a timestamp only if `clock` is given; `clock`
    ///   returns the time of day, like "12:03:16".
    pub fn new(out: &'a mut dyn Write, color: bool, columns: Option<fn() -> usize>, clock: Option<fn() -> String>) -> Printer<'a> {
        Printer { out, color, columns, clock, progress_chars: 0 }
    }

    /// `text` in `style`, if in color.
    fn paint(&self, style: Style, text: &str) -> String {
        if self.color { style.paint(text) } else { text.to_string() }
    }

    /// `value` in bold, if in color, to stand out in a plain line.  Not for
    /// other lines, since the reset at the end would also end a red line's
    /// color, and would throw off progress's count of characters.
    pub(crate) fn bold(&self, value: impl fmt::Display) -> String {
        self.paint(Bold, &value.to_string())
    }

    /// Prints `text` (in `style`, if in color) and a newline, in place of any
    /// progress on the last line.  If timestamped, the first line of `text`
    /// comes after the time and a space, and each later line of `text` comes
    /// after the same number of spaces, so that the lines of `text` line up.
    pub(crate) fn styled_line(&mut self, style: Style, text: &str) {
        if self.progress_chars > 0 {
            let _ = write!(self.out, "\r{}\r", " ".repeat(self.progress_chars));
            self.progress_chars = 0;
        }
        let text = if let Some(clock) = self.clock {
            let time = clock();
            let text = text.replace('\n', &format!("\n{}", " ".repeat(time.chars().count() + 1)));
            format!("{} {}", self.paint(Dim, &time), self.paint(style, &text))
        } else {
            self.paint(style, text)
        };
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    /// Prints `text` unstyled (see [`Printer::styled_line`]).
    pub(crate) fn line(&mut self, text: &str) {
        self.styled_line(Plain, text);
    }

    /// Prints `text` so that it's hard to miss.
    pub(crate) fn alarm(&mut self, text: &str) {
        let bar = "!".repeat(text.chars().count() + 8);
        self.styled_line(Red, &format!("{bar}\n!!! {text} !!!\n{bar}"));
    }

    /// If progress is shown (see [`Printer::new`]), shows `text` on the last
    /// line in place of the previous progress, after the time if timestamped
    /// and if that leaves room for some of `text`.  Whitespace at either end
    /// of `text` is dropped, each run of whitespace inside it (like a line
    /// break) becomes one space, and what's left is cut short with an
    /// ellipsis if it doesn't fit in the row.
    pub(crate) fn progress(&mut self, text: &str) {
        let Some(columns) = self.columns else { return };
        // \r only goes back to the start of the last row, so the line has to
        // fit in one, with room for the cursor.
        let width = columns().saturating_sub(1).max(1);
        // The time and a space, if that leaves room for some of the text
        let time = self.clock.map(|clock| clock()).filter(|time| time.chars().count() + 1 < width);
        let time_chars = time.as_ref().map_or(0, |time| time.chars().count() + 1);
        let text = human::truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), width - time_chars);
        let chars = time_chars + text.chars().count();
        let time = time.map_or(String::new(), |time| format!("{} ", self.paint(Dim, &time)));
        let _ = write!(self.out, "\r{time}{text}{}", " ".repeat(self.progress_chars.saturating_sub(chars)));
        let _ = self.out.flush();
        self.progress_chars = chars;
    }
}

impl Drop for Printer<'_> {
    /// Keeps the last progress, and ends its line so that whatever's printed
    /// next (like an error) starts on a line of its own.
    fn drop(&mut self) {
        if self.progress_chars > 0 {
            let _ = writeln!(self.out);
            let _ = self.out.flush();
        }
    }
}

/// `heading`, then each of `items` on an indented line.
pub(crate) fn indented_list(heading: &str, items: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let mut text = heading.to_string();
    for item in items {
        text.push_str("\n    ");
        text.push_str(item.as_ref());
    }
    text
}

/// The local time of day, like "12:03:16".
fn time_of_day() -> String {
    Zoned::now().strftime("%H:%M:%S").to_string()
}

/// The width and height of the terminal on stdout, in columns and rows, or
/// 80 columns and 24 rows for what's unknown.
pub(crate) fn terminal_size() -> (usize, usize) {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ fills in the winsize that its argument points to.
    let known = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    let columns = if known && size.ws_col > 0 { size.ws_col.into() } else { 80 };
    let rows    = if known && size.ws_row > 0 { size.ws_row.into() } else { 24 };
    (columns, rows)
}

impl Printer<'_> {
    /// Calls `print` with a [`Printer`] that writes to stdout, timestamps
    /// lines, styles the output if `color`, and shows progress if stdout is a
    /// terminal.  Returns what `print` returns.
    pub(crate) fn with_stdout<T>(color: bool, print: impl FnOnce(&mut Printer) -> T) -> T {
        let mut stdout = io::stdout();
        let columns = stdout.is_terminal().then_some((|| terminal_size().0) as fn() -> usize);
        print(&mut Printer::new(&mut stdout, color, columns, Some(time_of_day)))
    }
}

/// Lines on a terminal that are drawn over each time they change, by moving
/// the cursor up as many rows as they took (their own line breaks, and the
/// terminal's where they're too wide), and erasing from there down.  So
/// nothing else should be written to the terminal in the meantime, and the
/// cursor has to start at the beginning of a line.  Write errors are
/// ignored, as by [`Printer`].
pub(crate) struct LiveText<'a> {
    out: &'a mut dyn Write,
    /// Gives the terminal's width and height, in columns and rows, which can
    /// change.
    size: fn() -> (usize, usize),
    /// The lines drawn last, which are on the terminal above the cursor.
    drawn: String,
}

impl<'a> LiveText<'a> {
    /// Live text, none of it drawn yet, that goes to `out`, a terminal whose
    /// size `size` gives (see [`terminal_size`]).
    pub(crate) fn new(out: &'a mut dyn Write, size: fn() -> (usize, usize)) -> LiveText<'a> {
        LiveText { out, size, drawn: String::new() }
    }

    /// Draws `text` (lines that each end with a newline, and can be styled)
    /// in place of the lines drawn last; or if `text` takes up as many rows
    /// as the terminal has or more, which would leave the rows at its top out
    /// of reach the next time, only erases the lines drawn last.
    pub(crate) fn draw(&mut self, text: &str) {
        let (columns, rows) = (self.size)();
        let fits = text_rows(text, columns) < rows;
        self.replace(if fits { text } else { "" }, columns);
    }

    /// Draws `text` (as for [`LiveText::draw`]) in place of the lines drawn
    /// last, however many rows it takes up, and stops there, leaving it on
    /// the terminal.
    pub(crate) fn finish(mut self, text: &str) {
        let (columns, _) = (self.size)();
        self.replace(text, columns);
    }

    /// Erases the lines drawn last, which take up rows `columns` wide (the
    /// terminal rewraps lines when its width changes), and draws `text`.
    fn replace(&mut self, text: &str, columns: usize) {
        assert!(text.is_empty() || text.ends_with('\n'), "the last line doesn't end with a newline: {text:?}");
        let up = text_rows(&self.drawn, columns);
        let erase = if up > 0 { format!("\x1b[{up}A\x1b[J") } else { String::new() };
        let _ = self.out.write_all(format!("{erase}{text}").as_bytes());
        let _ = self.out.flush();
        self.drawn = text.to_string();
    }
}

/// How many rows of a terminal `columns` wide the lines in `text` take up:
/// at least one each, and more for a line too wide to fit in one, which the
/// terminal breaks into several.
fn text_rows(text: &str, columns: usize) -> usize {
    assert!(columns > 0, "a terminal without columns");
    text.lines().map(|line| human::styled_width(line).div_ceil(columns).max(1)).sum()
}

/// What `print` printed to a [`Printer`] with these settings (see
/// [`Printer::new`]), once the printer is dropped.
#[cfg(test)]
pub(crate) fn printed(color: bool, columns: Option<fn() -> usize>, clock: Option<fn() -> String>, print: impl FnOnce(&mut Printer)) -> String {
    let mut out = Vec::new();
    print(&mut Printer::new(&mut out, color, columns, clock));
    String::from_utf8(out).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_progress_over_itself() {
        let text = printed(false, Some(|| 80), None, |printer| {
            printer.line("start");
            printer.progress("12345");
            printer.progress("123");
            printer.line("done");
            printer.progress("last");
        });
        assert_eq!(text, "start\n\r12345\r123  \r   \rdone\n\rlast\n");
    }

    #[test]
    fn fits_progress_in_a_row() {
        let text = printed(false, Some(|| 6), None, |printer| {
            printer.progress("1234567");
            printer.line("done");
        });
        assert_eq!(text, "\r1234…\r     \rdone\n");
    }

    #[test]
    fn prints_no_progress_to_a_file() {
        let text = printed(false, None, None, |printer| {
            printer.line("start");
            printer.progress("12345");
            printer.alarm("bad");
        });
        assert_eq!(text, "start\n!!!!!!!!!!!\n!!! bad !!!\n!!!!!!!!!!!\n");
    }

    #[test]
    fn styles_values_and_problems() {
        let text = printed(true, None, None, |printer| {
            printer.line(&format!("{} is fine", printer.bold("one")));
            printer.styled_line(Red, &indented_list("problems:", ["one", "two"]));
        });
        assert_eq!(text, "\x1b[1mone\x1b[0m is fine\n\x1b[31mproblems:\n    one\n    two\x1b[0m\n");
        assert_eq!(printed(false, None, None, |printer| printer.line(&printer.bold("plain"))), "plain\n");
    }

    #[test]
    fn prints_timestamps() {
        let text = printed(true, Some(|| 20), Some(|| "12:34:56".into()), |printer| {
            printer.styled_line(Red, &indented_list("problems:", ["one"]));
            printer.progress("two\nlines");
            printer.progress("more than fits in a row");
        });
        let time = "\x1b[2m12:34:56\x1b[0m";
        assert_eq!(text, format!("{time} \x1b[31mproblems:\n             one\x1b[0m\n\r{time} two lines\r{time} more than…\n"));
        // Without room for the time and some of the text, just the text
        let text = printed(true, Some(|| 10), Some(|| "12:34:56".into()), |printer| printer.progress("cut short"));
        assert_eq!(text, "\rcut short\n");
    }

    #[test]
    fn draws_live_text_over_itself() {
        let mut out = Vec::new();
        let mut live = LiveText::new(&mut out, || (10, 5));
        live.draw("a\nb\n");
        // A line one column too wide for a row takes two, and the escape
        // sequences of a styled line take none.
        live.draw("0123456789x\n\x1b[31m0123456789\x1b[0m\n");
        // Five rows don't fit in five, as the cursor takes the row below
        live.draw("1\n2\n3\n4\n5\n");
        live.draw("\n");
        live.finish("1\n2\n3\n4\n5\n");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "a\nb\n\x1b[2A\x1b[J0123456789x\n\x1b[31m0123456789\x1b[0m\n\x1b[3A\x1b[J\n\x1b[1A\x1b[J1\n2\n3\n4\n5\n");
        // Rewrapped in a narrower terminal, the lines take more rows
        assert_eq!(text_rows("0123456789x\n", 4), 3);
    }

    #[test]
    fn tells_the_time() {
        let time = time_of_day();
        let shape = time.char_indices().all(|(i, c)| if i % 3 == 2 { c == ':' } else { c.is_ascii_digit() });
        assert!(time.len() == 8 && shape, "{time}");
    }
}
