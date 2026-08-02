//! Desktop notifications, through the freedesktop Desktop Notifications spec.
//!
//! `notify-send` is the reference client for `org.freedesktop.Notifications`,
//! the D-Bus interface every notification daemon implements: mako, dunst,
//! GNOME Shell, Plasma, xfce4-notifyd. Nothing here is tied to any one of them;
//! the desktop's own daemon decides how it looks, where it appears, and how
//! long it stays.
//!
//! Shelled out rather than speaking D-Bus directly, for the same reason
//! `xdg-open` and `wl-copy` are: a D-Bus client library is a large dependency
//! for one call, and libnotify ships on any desktop that has a daemon to talk
//! to. A missing binary means no notification and nothing else, this is a
//! courtesy, not a feature worth failing a transfer over.

use std::process::{Command, Stdio};

use gpui::{App, prelude::*};

/// How long a transfer must run before finishing it is worth interrupting for.
///
/// Anything quicker than this finished while you were still looking at it, and
/// the job strip already said so. The point of a notification is the transfer
/// you walked away from.
pub const NOTIFY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// Standard categories from the spec, which daemons may use for filtering,
/// grouping, or an icon of their own choosing.
const CATEGORY_OK: &str = "transfer.complete";
const CATEGORY_ERR: &str = "transfer.error";

/// Whether a finished job is worth interrupting the desktop for.
///
/// Cancelling is something you just did, so being told about it is noise.
/// A job shorter than `NOTIFY_AFTER` finished while you were still looking at
/// it, and the job strip already said so. Errors always announce however brief
/// the job, the strip keeps a failed transfer until it is dismissed, but only
/// a notification reaches you when hoja is not the window you are looking at.
pub fn worth_announcing(
    outcome: hoja_transfer::Outcome,
    errors: usize,
    elapsed: std::time::Duration,
) -> bool {
    if outcome == hoja_transfer::Outcome::Cancelled {
        return false;
    }
    errors > 0 || elapsed >= NOTIFY_AFTER
}

/// Post a notification about a finished transfer.
///
/// Runs on the background executor: the call is fast, but it is still a process
/// spawn and a D-Bus round trip, and neither belongs on the frame path.
pub fn transfer_finished(summary: String, body: String, failed: bool, cx: &App) {
    cx.background_spawn(async move {
        // `output()` rather than `spawn()`: it reaps. A detached notify-send
        // would otherwise sit as a zombie until hoja exits, once per transfer.
        let _ = Command::new("notify-send")
            .args([
                "--app-name=hoja",
                // The same name the desktop entry uses, so the notification
                // carries whatever the active icon theme draws for a file
                // manager rather than a generic placeholder.
                "--icon=system-file-manager",
                if failed {
                    "--urgency=critical"
                } else {
                    "--urgency=normal"
                },
            ])
            .arg(format!(
                "--category={}",
                if failed { CATEGORY_ERR } else { CATEGORY_OK }
            ))
            .arg(summary)
            .arg(body)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    })
    .detach();
}

/// "1,234 files": thousands separated, because the counts here run to six
/// digits and a bare number that long is unreadable at a glance.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use hoja_transfer::Outcome;
    use std::time::Duration;

    #[test]
    fn only_announces_what_you_would_want_to_hear_about() {
        let long = NOTIFY_AFTER + Duration::from_secs(1);
        let short = Duration::from_millis(200);

        assert!(worth_announcing(Outcome::Completed, 0, long), "you walked away");
        assert!(!worth_announcing(Outcome::Completed, 0, short), "you watched it happen");
        assert!(
            worth_announcing(Outcome::CompletedWithErrors, 3, short),
            "errors announce however quick, since you may not be looking"
        );
        assert!(
            !worth_announcing(Outcome::Cancelled, 0, long),
            "you cancelled it yourself"
        );
        assert!(
            !worth_announcing(Outcome::Cancelled, 2, long),
            "still yours; the errors are a consequence of cancelling"
        );
    }

    #[test]
    fn counts_group_into_thousands() {
        assert_eq!(count(0), "0");
        assert_eq!(count(7), "7");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(74966), "74,966");
        assert_eq!(count(1234567), "1,234,567");
    }
}
