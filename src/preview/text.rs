//! The first lines of a text file, cut to fit a tooltip.

use std::io::Read;
use std::path::Path;

/// Bytes read from the start of the file. Enough for `LINES` long lines, and
/// small enough that hovering a 4 GB log reads no more than any other file.
const READ: usize = 8 * 1024;
/// Lines kept.
pub const LINES: usize = 16;
/// Characters kept per line, after tabs are expanded. Eighty, so a full line
/// in the preview's small mono face fits the tooltip's width: at a hundred
/// it ran past the edge.
pub const COLUMNS: usize = 80;
const TAB: usize = 4;

/// The excerpt, or `None` when the bytes do not read as text.
pub fn excerpt(path: &Path) -> Option<Vec<String>> {
    let mut bytes = Vec::with_capacity(READ);
    std::fs::File::open(path)
        .ok()?
        .take(READ as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    lines_of(&bytes, bytes.len() == READ)
}

/// `cut` says the read stopped at `READ`, so a broken UTF-8 sequence at the
/// very end is the cut through a character, not a sign of binary data.
fn lines_of(bytes: &[u8], cut: bool) -> Option<Vec<String>> {
    // A NUL is the cheapest sign of binary data that is reliable, and it is
    // the one `grep` and `git` use.
    if bytes.contains(&0) {
        return None;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        // `error_len() == None` means the input ended in the middle of a
        // sequence, which is what a cut through a character looks like.
        Err(err) if cut && err.error_len().is_none() => {
            std::str::from_utf8(&bytes[..err.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    Some(text.lines().take(LINES).map(fit).collect())
}

/// Tabs expanded to the next stop, then cut at `COLUMNS` characters. Counted
/// in characters, so a cut never lands inside one.
fn fit(line: &str) -> String {
    let mut out = String::new();
    let mut column = 0;
    for ch in line.chars() {
        if column >= COLUMNS {
            out.push('…');
            break;
        }
        match ch {
            '\t' => {
                let stop = (column / TAB + 1) * TAB;
                while column < stop.min(COLUMNS) {
                    out.push(' ');
                    column += 1;
                }
            }
            // A carriage return from a CRLF file, and other control
            // characters, would draw as boxes.
            ch if ch.is_control() => {}
            ch => {
                out.push(ch);
                column += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_go_to_the_next_stop() {
        assert_eq!(fit("a\tb"), "a   b");
        assert_eq!(fit("\tx"), "    x");
        assert_eq!(fit("abcd\te"), "abcd    e");
    }

    #[test]
    fn a_long_line_is_cut_with_a_mark() {
        let line = "x".repeat(COLUMNS + 20);
        let out = fit(&line);
        assert_eq!(out.chars().count(), COLUMNS + 1);
        assert!(out.ends_with('…'));
        assert_eq!(fit(&"y".repeat(COLUMNS)), "y".repeat(COLUMNS));
    }

    #[test]
    fn a_wide_character_is_counted_once() {
        let line = "é".repeat(COLUMNS + 1);
        assert_eq!(fit(&line).chars().count(), COLUMNS + 1);
    }

    #[test]
    fn a_nul_means_binary() {
        assert_eq!(lines_of(b"ELF\0\x01", false), None);
    }

    #[test]
    fn a_character_cut_by_the_read_is_not_binary() {
        // "é" is C3 A9. Stop the read after C3.
        let mut bytes = b"line one\ncaf".to_vec();
        bytes.push(0xC3);
        assert_eq!(
            lines_of(&bytes, true),
            Some(vec!["line one".into(), "caf".into()])
        );
        // The same bytes from a file that ended there are not text.
        assert_eq!(lines_of(&bytes, false), None);
    }

    #[test]
    fn invalid_utf8_in_the_middle_is_binary() {
        assert_eq!(lines_of(b"ab\xFFcd", true), None);
    }

    #[test]
    fn crlf_and_line_count() {
        let text = (0..40).map(|n| format!("{n}\r\n")).collect::<String>();
        let lines = lines_of(text.as_bytes(), false).unwrap();
        assert_eq!(lines.len(), LINES);
        assert_eq!(lines[0], "0");
    }
}
