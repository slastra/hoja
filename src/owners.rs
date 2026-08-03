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

/// The user's name, or the number when nothing claims it.
pub fn user(uid: u32) -> String {
    static USERS: OnceLock<HashMap<u32, String>> = OnceLock::new();
    USERS
        .get_or_init(|| read("/etc/passwd"))
        .get(&uid)
        .cloned()
        .unwrap_or_else(|| uid.to_string())
}

/// The group's name, or the number when nothing claims it.
pub fn group(gid: u32) -> String {
    static GROUPS: OnceLock<HashMap<u32, String>> = OnceLock::new();
    GROUPS
        .get_or_init(|| read("/etc/group"))
        .get(&gid)
        .cloned()
        .unwrap_or_else(|| gid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
