//! What a transfer is doing, written down while it does it.
//!
//! Only enough to pick it up again: which files, where they were going, and
//! which run was moving them. Undo needs far more than this and keeps it in
//! memory, because undo is a live-session affordance — the stack already dies
//! with the process, and ctrl-z reaching back into a session nobody remembers
//! would be a surprise rather than a feature. What survives a crash is only
//! what is needed to answer "there was a transfer here, shall I finish it?".
//!
//! So this costs two small writes a job, whatever its size: one when it
//! starts and one to remove when it stops. A record for two hundred thousand
//! files is the same two hundred bytes as a record for one.
//!
//! # Not JSON
//!
//! `serde_json` cannot serialize a path. `PathBuf`'s `Serialize` goes through
//! `to_str` and *fails* on bytes that are not UTF-8, and such names are legal
//! on ext4 and are shown in every listing hoja draws. A record that could not
//! be written for a file the user is looking at would be a record that fails
//! exactly when it is needed. So this is line-oriented text with the paths
//! percent-encoded by the same routine the trash spec already made necessary,
//! which is bytes-exact and needs no dependency.
//!
//! ```text
//! hoja-job 1
//! pid 48211
//! op copy
//! dest /home/s/dst
//! src /home/s/src/tree
//! src /home/s/src/other%FF
//! ```
//!
//! Unknown keys are skipped and an unrecognised version is left alone
//! entirely, so a newer hoja's records are not acted on by an older one.

use std::path::{Path, PathBuf};

use hoja_transfer::{JobSpec, Operation};

/// Bumped only when an older hoja must not act on a newer record. A reader
/// that does not recognise the version leaves the file where it is, which
/// includes leaving its partial files alone.
const VERSION: &str = "hoja-job 1";

/// A transfer that was running when its record was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub pid: u32,
    /// What the user answered about collisions, so finishing the job asks the
    /// same question the same way rather than quietly deciding for them.
    pub conflict: Option<hoja_transfer::ConflictChoice>,
    /// What this run stamps into the names of its half-written files. A pid
    /// can be recycled and a token cannot, so this is what the reaper matches.
    pub token: u64,
    pub op: Operation,
    pub dest: PathBuf,
    pub sources: Vec<PathBuf>,
}

/// Where records live. Its own directory, not a key in `state.json`: that is a
/// debounced whole-document read-modify-write under a lock shared by every
/// window, and these have exactly one writer each and want no merging.
pub fn dir() -> Option<PathBuf> {
    Some(crate::config::state_home()?.join("hoja/jobs"))
}

impl Record {
    fn render(&self) -> String {
        let mut out = format!(
            "{VERSION}\npid {}\ntoken {:x}\nconflict {}\nop {}\ndest {}\n",
            self.pid,
            self.token,
            match self.conflict {
                None => "ask",
                Some(hoja_transfer::ConflictChoice::Overwrite) => "overwrite",
                Some(hoja_transfer::ConflictChoice::Skip) => "skip",
                Some(hoja_transfer::ConflictChoice::KeepBoth) => "both",
            },
            match self.op {
                Operation::Copy => "copy",
                Operation::Move => "move",
            },
            hoja_transfer::percent_encode(&self.dest),
        );
        for source in &self.sources {
            out.push_str(&format!("src {}\n", hoja_transfer::percent_encode(source)));
        }
        out
    }

    fn parse(text: &str) -> Option<Record> {
        let mut lines = text.lines();
        if lines.next()? != VERSION {
            return None;
        }
        let mut pid = None;
        let mut token = None;
        let mut conflict = None;
        let mut op = None;
        let mut dest = None;
        let mut sources = Vec::new();
        for line in lines {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            match key {
                "pid" => pid = value.parse().ok(),
                "token" => token = u64::from_str_radix(value, 16).ok(),
                "conflict" => {
                    conflict = Some(match value {
                        "overwrite" => Some(hoja_transfer::ConflictChoice::Overwrite),
                        "skip" => Some(hoja_transfer::ConflictChoice::Skip),
                        "both" => Some(hoja_transfer::ConflictChoice::KeepBoth),
                        _ => None,
                    })
                }
                "op" => {
                    op = match value {
                        "copy" => Some(Operation::Copy),
                        "move" => Some(Operation::Move),
                        _ => None,
                    }
                }
                "dest" => dest = Some(hoja_transfer::percent_decode(value)),
                "src" => sources.push(hoja_transfer::percent_decode(value)),
                // Skipped rather than refused, so a field added later does not
                // make this unreadable to the version that came before it.
                _ => {}
            }
        }
        Some(Record {
            pid: pid?,
            token: token?,
            // An older record has no line for it, and "ask" is the answer that
            // decides nothing on the user's behalf.
            conflict: conflict.unwrap_or(None),
            op: op?,
            dest: dest?,
            sources,
        })
    }

    /// The spec that would finish what this record started.
    ///
    /// Sources that are no longer there are dropped: a move takes them as it
    /// goes, so re-running with the original list would raise a walk error for
    /// every file it had already dealt with. `None` when there is nothing left
    /// to do.
    pub fn remaining(&self) -> Option<JobSpec> {
        let sources: Vec<PathBuf> = self
            .sources
            .iter()
            .filter(|source| source.symlink_metadata().is_ok())
            .cloned()
            .collect();
        if sources.is_empty() || !self.dest.is_dir() {
            return None;
        }
        Some(JobSpec {
            op: self.op,
            sources,
            dest_dir: self.dest.clone(),
            // A resumed job's sources are wherever they always were. An
            // extraction's scratch directory does not survive a crash, and
            // `remaining` drops sources that are gone, so nothing here can
            // still be staged.
            staging: None,
            policy: hoja_transfer::JobPolicy {
                // The answer the user gave the first time, not Skip.
                //
                // Skip looks right — most of what is at the destination is
                // what the dead run put there — and is wrong wherever the
                // destination already held files of the same name. Someone who
                // answered "replace all" and was interrupted halfway would
                // have got a job that reported finishing while half the files
                // they asked to replace still held their old contents. Where
                // they were never asked, they are asked now.
                conflict: self.conflict,
                abort_on_first_error: false,
            },
        })
    }
}

/// A record whose owner is still running holds its own lock; ours holds it for
/// as long as the job does, and drops it — releasing the lock and removing the
/// file — when the job ends.
pub struct Entry {
    path: PathBuf,
    /// Held open purely for the `flock` on it.
    _lock: std::fs::File,
}

impl Entry {
    /// Write a record for a job that is starting.
    ///
    /// `None` if there is nowhere to write one. A transfer must never fail
    /// because its bookkeeping could not be set up, so every caller of this
    /// ignores the answer and carries on.
    pub fn open(spec: &JobSpec, at: std::time::SystemTime) -> Option<Entry> {
        Entry::open_in(&dir()?, spec, at)
    }

    /// The testable core. Takes the directory rather than reaching for
    /// `$XDG_STATE_HOME`, which is process-wide and would have to be set to
    /// test any of this — the same reason `config::State::commit_at` exists.
    fn open_in(dir: &Path, spec: &JobSpec, at: std::time::SystemTime) -> Option<Entry> {
        use std::io::Write;

        std::fs::create_dir_all(dir).ok()?;
        let pid = std::process::id();
        // Timestamp first so the directory sorts chronologically, which is the
        // order they would have to be offered back in. The counter after it is
        // what keeps two jobs started in the same millisecond — two panes, a
        // held key, a drag onto a split — from naming the same file: the
        // second used to truncate the first's record and then fail to lock it,
        // leaving one job describing the other and the other describing
        // nothing.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = at.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
        let path = dir.join(format!("{stamp:013}-{pid}-{seq}.job"));

        let record = Record {
            pid,
            token: hoja_transfer::run_token(),
            conflict: spec.policy.conflict,
            op: spec.op,
            dest: spec.dest_dir.clone(),
            sources: spec.sources.clone(),
        };

        // Created, locked, and only then written. The old order wrote first and
        // locked after, which left two windows where the file existed with
        // nobody holding it: a failure in between abandoned a record for a job
        // that was about to run, and even on the way to success another hoja
        // starting in that instant would have taken the lock, reaped this job's
        // partial files out from under it, and offered to finish a transfer
        // that had not started. `create_new` also makes a name collision an
        // error rather than a silent truncation of somebody else's record.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()?;
        let locked = rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .is_ok()
            && file.write_all(record.render().as_bytes()).is_ok();
        if !locked {
            // Never leave a record nobody holds: the next start would treat it
            // as the remains of a crash.
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some(Entry { path, _lock: file })
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Every record left behind by a run that is no longer here.
///
/// Ownership is the `flock`, not the pid. A pid can be recycled, so "no
/// process has this number" is not proof; a lock that can be taken is, because
/// only the owner holds it and only death releases it. A record we cannot lock
/// belongs to another hoja that is running right now, and is left alone.
pub fn abandoned() -> Vec<(PathBuf, Record)> {
    match dir() {
        Some(dir) => abandoned_in(&dir),
        None => Vec::new(),
    }
}

fn abandoned_in(dir: &Path) -> Vec<(PathBuf, Record)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, Record)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("job") {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) else {
            continue;
        };
        if rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
            continue; // A live hoja owns this one.
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match Record::parse(&text) {
            Some(record) => found.push((path, record)),
            // Unreadable, or written by a version this one does not know. Left
            // in place: deleting a record means deleting the only evidence of
            // what its partial files belong to.
            None => continue,
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Remove the half-written files an abandoned run left in its destination.
///
/// Exact rather than a guess, and both halves matter. The lock already proved
/// the writer is gone, and the name carries the pid that wrote it, so only
/// that run's leftovers go. A partial with no record — one from a hoja that
/// predates any of this — is left alone, because removing a hidden file on
/// suspicion is worse than leaving it.
///
/// Bounded by the destination tree, since that is the only place a job writes.
pub fn reap(record: &Record) -> usize {
    let mut removed = 0;
    let mut stack = vec![record.dest.clone()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if hoja_transfer::partial_token(&name) == Some(record.token) {
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
                continue;
            }
            // `file_type` from the directory entry, which does not follow a
            // symlink, where `path.is_dir()` does. Following one took the walk
            // out of the destination tree it is documented to stay inside, and
            // a link back up it — `latest -> .`, which hoja copies verbatim —
            // made this descend forever, before the window had opened.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {

    use super::*;

    fn record(dest: &std::path::Path) -> Record {
        Record {
            pid: 4242,
            token: 0xfeed_beef,
            conflict: None,
            op: Operation::Copy,
            dest: dest.to_path_buf(),
            sources: vec![PathBuf::from("/a/one"), PathBuf::from("/a/two")],
        }
    }

    #[test]
    fn a_record_round_trips() {
        let original = record(Path::new("/dest"));
        assert_eq!(Record::parse(&original.render()), Some(original));
    }

    #[test]
    fn a_path_that_is_not_utf8_survives_the_record() {
        use std::os::unix::ffi::OsStrExt;
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/src/\xff\xfe name"));
        let original = Record {
            sources: vec![odd.clone()],
            ..record(Path::new("/dest"))
        };
        assert_eq!(
            Record::parse(&original.render()).unwrap().sources,
            vec![odd]
        );
    }

    #[test]
    fn a_record_from_a_version_this_one_does_not_know_is_refused() {
        // Refused rather than guessed at: acting on a newer record could mean
        // reaping files by a rule that has changed.
        let text = record(Path::new("/dest")).render().replacen("1", "2", 1);
        assert_eq!(Record::parse(&text), None);
    }

    #[test]
    fn an_unknown_field_is_skipped_rather_than_fatal() {
        let text = format!("{}colour blue\n", record(Path::new("/dest")).render());
        assert!(Record::parse(&text).is_some());
    }

    #[test]
    fn what_is_left_drops_the_sources_that_are_gone() {
        let dir = std::env::temp_dir().join(format!("hoja-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let here = dir.join("still-here");
        std::fs::write(&here, b"x").unwrap();

        let record = Record {
            pid: 1,
            token: 0xfeed_beef,
            conflict: None,
            op: Operation::Move,
            dest: dir.clone(),
            sources: vec![here.clone(), dir.join("already-moved")],
        };
        let spec = record.remaining().expect("something is left");
        assert_eq!(spec.sources, vec![here], "only what is still there");

        let nothing = Record {
            sources: vec![dir.join("gone")],
            ..record
        };
        assert!(nothing.remaining().is_none(), "and nothing means nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reaper_takes_only_its_own() {
        let dir = std::env::temp_dir().join(format!("hoja-reap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("down/deeper")).unwrap();

        // One of ours, one from another run, one ordinary file, one nested.
        std::fs::write(dir.join(".hoja-partial-a.txt.4242-feedbeef-1"), b"").unwrap();
        std::fs::write(dir.join(".hoja-partial-b.txt.9999-deadbeef-1"), b"").unwrap();
        std::fs::write(dir.join("ordinary.txt"), b"").unwrap();
        std::fs::write(
            dir.join("down/deeper/.hoja-partial-c.txt.4242-feedbeef-2"),
            b"",
        )
        .unwrap();

        // And one behind a symlink that points back at the top, which is what
        // used to make this descend forever.
        std::os::unix::fs::symlink(&dir, dir.join("down/latest")).unwrap();

        assert_eq!(reap(&record(&dir)), 2, "both of this run's, at any depth");
        assert!(!dir.join(".hoja-partial-a.txt.4242-feedbeef-1").exists());
        assert!(
            !dir.join("down/deeper/.hoja-partial-c.txt.4242-feedbeef-2")
                .exists()
        );
        assert!(
            dir.join(".hoja-partial-b.txt.9999-deadbeef-1").exists(),
            "another run's is not ours to remove"
        );
        assert!(dir.join("ordinary.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hoja-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec(dest: &Path) -> JobSpec {
        JobSpec {
            op: Operation::Copy,
            sources: vec![dest.join("a")],
            dest_dir: dest.to_path_buf(),
            policy: Default::default(),
            staging: None,
        }
    }

    #[test]
    fn a_running_job_is_not_abandoned_and_stops_being_a_record_when_it_ends() {
        // Ownership is the lock, not the pid: this process is very much alive,
        // and the proof of that available to another hoja is that the lock
        // cannot be taken.
        let dir = scratch("journal-live");
        let entry = Entry::open_in(&dir, &spec(&dir), std::time::SystemTime::now())
            .expect("a record was written");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "the record is there while the job runs"
        );
        assert!(
            abandoned_in(&dir).is_empty(),
            "and it is nobody else's to pick up while we hold it"
        );

        drop(entry);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "ending the job takes the record with it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_jobs_in_the_same_millisecond_get_records_of_their_own() {
        // The name used to be the timestamp and the pid, so a second paste in
        // the same millisecond truncated the first's record and then failed to
        // lock it — one job describing the other, and the other describing
        // nothing at all.
        let dir = scratch("journal-same-ms");
        let at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000);
        let first = Entry::open_in(&dir, &spec(&dir), at).expect("the first is written");
        let second = Entry::open_in(&dir, &spec(&dir), at).expect("and so is the second");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "two jobs, two records"
        );
        assert!(abandoned_in(&dir).is_empty(), "and both are held");
        drop(first);
        drop(second);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_is_never_left_where_nobody_holds_it() {
        // Written under the lock rather than before taking it. A record that
        // exists unlocked is, to the next start, the remains of a crash: it
        // reaps that job's partial files and offers to finish a transfer that
        // may be running right now.
        let dir = scratch("journal-unheld");
        let entry = Entry::open_in(&dir, &spec(&dir), std::time::SystemTime::now()).unwrap();
        let held: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(held.len(), 1);
        assert!(
            abandoned_in(&dir).is_empty(),
            "no window in which it is on disk and unheld"
        );
        drop(entry);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_nobody_holds_is_there_to_pick_up() {
        let dir = scratch("journal-dead");
        let dest = scratch("journal-dead-dest");
        // As a dead run would have left it: written, never removed, unlocked.
        let record = Record {
            pid: 4242,
            token: 0xfeed_beef,
            conflict: None,
            op: Operation::Move,
            dest: dest.clone(),
            sources: vec![dest.join("x")],
        };
        std::fs::write(dir.join("0000000000001-4242.job"), record.render()).unwrap();

        let found = abandoned_in(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, record);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn something_that_is_not_a_record_is_left_where_it_is() {
        let dir = scratch("journal-junk");
        std::fs::write(dir.join("0000000000001-1.job"), "who knows\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "not a record").unwrap();
        assert!(abandoned_in(&dir).is_empty());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "and neither is deleted: a record is the only evidence of what its \
             partial files belong to"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn what_the_user_answered_survives_the_crash() {
        // Skip looks right — most of what is at the destination is what the
        // dead run put there — and is wrong wherever the destination already
        // held files of the same name. Somebody who answered "replace all" and
        // was interrupted halfway would otherwise get a job that reported
        // finishing while half the files they asked to replace still held
        // their old contents.
        let dir = std::env::temp_dir().join(format!("hoja-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let one = dir.join("one");
        std::fs::write(&one, b"x").unwrap();

        for answer in [
            None,
            Some(hoja_transfer::ConflictChoice::Overwrite),
            Some(hoja_transfer::ConflictChoice::KeepBoth),
            Some(hoja_transfer::ConflictChoice::Skip),
        ] {
            let record = Record {
                pid: 1,
                token: 7,
                conflict: answer,
                op: Operation::Copy,
                dest: dir.clone(),
                sources: vec![one.clone()],
            };
            assert_eq!(
                Record::parse(&record.render()).unwrap().conflict,
                answer,
                "the answer has to survive being written down"
            );
            assert_eq!(
                record.remaining().unwrap().policy.conflict,
                answer,
                "and reach the job that finishes the work"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_written_before_the_answer_was_kept_asks() {
        // No `conflict` line at all, as an older hoja would have left it.
        // Asking is the only answer that decides nothing on the user's behalf.
        let text = "hoja-job 1\npid 1\ntoken 7\nop copy\ndest /d\nsrc /s\n";
        assert_eq!(Record::parse(text).unwrap().conflict, None);
    }
}
