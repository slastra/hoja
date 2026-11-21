//! A PDF's page count and first page shape, from Poppler's `pdfinfo`.
//!
//! Poppler is what the Evince and Papers thumbnailers draw with, so the page
//! `pdfinfo` measures is the page the thumbnail shows. It reads the cross
//! reference table and the objects it needs, not the whole file. Without it,
//! a PDF still gets its picture: only the page count is missing, and the
//! place kept for the picture is a portrait Letter page.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// `pdfinfo` answers in tens of milliseconds. A file that takes longer than
/// this is one it is struggling with, and the picture will say the rest.
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pdf {
    pub pages: u32,
    /// Width over height of the first page as shown, after its rotation.
    pub aspect: Option<f32>,
}

/// `None` when `pdfinfo` is not installed, fails, or takes too long.
pub fn read(path: &Path) -> Option<Pdf> {
    let mut child = Command::new("pdfinfo")
        .args(["-enc", "UTF-8"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // The report is a few hundred bytes, far less than a pipe holds, so the
    // child never blocks on a full pipe while this waits for it to exit.
    if !super::thumbnail::wait(child, TIMEOUT) {
        return None;
    }
    let mut report = String::new();
    stdout.read_to_string(&mut report).ok()?;
    parse(&report)
}

fn parse(report: &str) -> Option<Pdf> {
    let mut pages = None;
    let mut size = None;
    let mut rotation = 0;
    for line in report.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "Pages" => pages = value.parse::<u32>().ok(),
            // "612 x 792 pts (letter)": the name, when there is one, is after.
            "Page size" => {
                let mut words = value.split_whitespace();
                let width = words.next()?.parse::<f32>().ok();
                let height = words.nth(1)?.parse::<f32>().ok();
                size = width.zip(height).filter(|(w, h)| *w > 0. && *h > 0.);
            }
            "Page rot" => rotation = value.parse::<i32>().unwrap_or(0),
            _ => {}
        }
    }
    let aspect = size.map(|(w, h)| {
        if rotation.rem_euclid(180) == 90 {
            h / w
        } else {
            w / h
        }
    });
    Some(Pdf {
        pages: pages?,
        aspect,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_is_read() {
        let pdf = parse(
            "Title:           Minutes\n\
             Pages:           12\n\
             Encrypted:       no\n\
             Page size:       612 x 792 pts (letter)\n\
             Page rot:        0\n",
        )
        .unwrap();
        assert_eq!(pdf.pages, 12);
        assert!((pdf.aspect.unwrap() - 612. / 792.).abs() < 1e-5);
    }

    #[test]
    fn a_turned_page_is_wide() {
        let pdf = parse("Pages: 1\nPage size: 595.28 x 841.89 pts (A4)\nPage rot: 270\n").unwrap();
        assert!(pdf.aspect.unwrap() > 1.);
    }

    #[test]
    fn a_title_with_a_colon_is_not_a_key() {
        let pdf = parse("Title: Page size: a novel\nPages: 3\n").unwrap();
        assert_eq!(pdf.pages, 3);
        assert_eq!(pdf.aspect, None);
    }

    #[test]
    fn no_page_count_is_no_answer() {
        assert_eq!(
            parse("Syntax Error: Couldn't find trailer dictionary\n"),
            None
        );
    }
}
