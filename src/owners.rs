//! Names for the numbers in `st_uid` and `st_gid`.
//!
//! Read once from `/etc/passwd` and `/etc/group` and kept. Not `getpwuid`,
//! which would be the thorough answer: that goes through NSS and so also
//! resolves users from LDAP or SSSD, but it is not in `rustix`, and reaching
//! for `libc` to name the owner of a file in a status line is a dependency for
//! a footnote.
//!
//! The cost of the shortcut is that a directory owned by a network account
//! shows the number instead of the name, and that a user added while hoja is
//! running is not seen. Both read as a number rather than as a wrong name,
//! which is the failure worth having.

use std::collections::HashMap;
use std::sync::OnceLock;

/// `name:x:uid:...` from a colon-separated database.
fn read(path: &str) -> HashMap<u32, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let name = fields.next()?;
            let _password = fields.next()?;
            let id: u32 = fields.next()?.parse().ok()?;
            Some((id, name.to_string()))
        })
        .collect()
}

fn users() -> &'static HashMap<u32, String> {
    static USERS: OnceLock<HashMap<u32, String>> = OnceLock::new();
    USERS.get_or_init(|| read("/etc/passwd"))
}

fn groups() -> &'static HashMap<u32, String> {
    static GROUPS: OnceLock<HashMap<u32, String>> = OnceLock::new();
    GROUPS.get_or_init(|| read("/etc/group"))
}

/// The user's name, or `None` when nothing claims the id.
///
/// Borrowed rather than cloned, because the map lives for the run and the
/// Owner column's sort compares one of these on every one of ~n log n
/// comparisons. `user` is the same lookup with the number substituted, which
/// is what a cell prints and what cannot be borrowed from anywhere.
pub fn user_name(uid: u32) -> Option<&'static str> {
    users().get(&uid).map(String::as_str)
}

/// The group's name, or `None` when nothing claims the id.
pub fn group_name(gid: u32) -> Option<&'static str> {
    groups().get(&gid).map(String::as_str)
}

/// The user's name, or the number when nothing claims it.
pub fn user(uid: u32) -> String {
    user_name(uid)
        .map(str::to_string)
        .unwrap_or_else(|| uid.to_string())
}

/// The group's name, or the number when nothing claims it.
pub fn group(gid: u32) -> String {
    group_name(gid)
        .map(str::to_string)
        .unwrap_or_else(|| gid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unclaimed_id_has_no_name_to_borrow() {
        // What the sort keys off. `None` rather than the number, so a column
        // sorted by owner can put the unnamed ones together instead of
        // interleaving them with names that happen to start with a digit.
        assert_eq!(user_name(4_000_000_000), None);
        assert_eq!(group_name(4_000_000_000), None);
    }

    #[test]
    fn an_unclaimed_id_reads_as_its_number() {
        // Nothing owns this, on any machine this will run on.
        assert_eq!(user(4_000_000_000), "4000000000");
        assert_eq!(group(4_000_000_000), "4000000000");
    }

    #[test]
    fn root_is_named_where_the_database_is_readable() {
        // Every passwd has uid 0, and it is the one entry whose name is fixed.
        // Skipped rather than failed where the file cannot be read at all, so
        // this says something about parsing and nothing about the environment.
        if std::fs::read_to_string("/etc/passwd").is_ok() {
            assert_eq!(user(0), "root");
        }
    }
}
