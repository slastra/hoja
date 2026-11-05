//! Taking a finished transfer back: the log a job returns, replayed in
//! reverse by a job of its own.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::{Release, Step, Worker};
use crate::events::{Event, JobSummary, Outcome, Stage, Undone};
use crate::job::Phase;
use crate::{meta, sys};

impl Worker {
    /// Walk the log backwards, reversing what it can.
    ///
    /// Backwards because the records of one overwrite are the old file
    /// displaced and then the new one created, and putting the old one back
    /// has to happen after the new one is out of the way — `restore` refuses
    /// to clobber, so the other order would simply fail.
    pub(super) fn run_undo(&mut self, records: Vec<Undone>) {
        self.progress
            .files_total
            .store(records.len() as u64, Ordering::Relaxed);
        // Nothing to walk: the denominator is known before the first step, so
        // the bar is real from the start.
        self.progress.walk_complete.store(true, Ordering::Relaxed);
        self.progress.set_phase(Phase::Transferring);

        let mut cancelled = false;
        let mut outstanding = Vec::new();
        let mut left = records.into_iter().rev();
        for record in left.by_ref() {
            if self.check_pause_cancel() {
                cancelled = true;
                outstanding.push(record);
                break;
            }
            if !self.undo_one(&record) {
                outstanding.push(record);
            }
            self.progress.files_done.fetch_add(1, Ordering::Relaxed);
        }
        // Everything the loop never reached is still owed. Breaking out of a
        // consuming iterator drops the rest of it, so cancelling halfway
        // through undoing two hundred thousand files used to lose the record
        // of the hundred thousand not yet reversed — leaving them at the
        // destination with nothing anywhere that knew they were there.
        outstanding.extend(left);
        // The copy-back of a cross-filesystem move waits on a sync before it
        // removes anything, the same as the move it is reversing.
        self.flush_sources();
        // Directories that were to go once their children did, and could not.
        // Last in processing order, so first once reversed below: a directory
        // is recorded before anything in it.
        outstanding.append(&mut self.owed);

        self.progress.set_phase(Phase::Finished);
        // Back into the order they were recorded in, so what goes onto the
        // stack can be replayed by a second press exactly as this one was.
        outstanding.reverse();
        let summary = JobSummary {
            outcome: if cancelled {
                Outcome::Cancelled
            } else if self.errors.is_empty() {
                Outcome::Completed
            } else {
                Outcome::CompletedWithErrors
            },
            errors: std::mem::take(&mut self.errors),
            files_copied: self.files_copied,
            files_skipped: self.files_skipped,
            bytes_copied: self.progress.bytes_done.load(Ordering::Relaxed),
            stats: self.stats,
            undone: outstanding,
            undoable: false,
        };
        let _ = self.events.send(Event::Done(summary));
    }

    /// Reverse one record. `false` means it is still outstanding.
    fn undo_one(&mut self, record: &Undone) -> bool {
        match record {
            Undone::Displaced(item) => match crate::trash::restore(item) {
                Ok(()) => {
                    self.files_copied += 1;
                    true
                }
                Err(err) => {
                    self.queue_error(&item.original, Stage::Rename, err);
                    false
                }
            },
            Undone::Lost(path) => {
                self.queue_error(
                    path,
                    Stage::Write,
                    std::io::Error::other(
                        "this was replaced on a filesystem with no trash, so what was here was not kept",
                    ),
                );
                false
            }
            Undone::Renamed { from, to, dev, ino } => {
                if !self.still_the_same(to, *dev, *ino) {
                    return false;
                }
                match sys::rename_no_replace(to, from) {
                    Ok(()) => {
                        self.stats.renames += 1;
                        self.files_copied += 1;
                        true
                    }
                    Err(err) => {
                        self.queue_error(to, Stage::Rename, err);
                        false
                    }
                }
            }
            Undone::RemovedDir { path, mode, mtime } => match std::fs::create_dir_all(path) {
                Ok(()) => {
                    // A directory made afresh is 0777 & ~umask, so putting a
                    // 0700 one back without this widened it — a keystroke
                    // meant to be conservative quietly opening private files.
                    let outcome = meta::restore_dir_meta(path, *mode, *mtime);
                    for w in outcome.warnings {
                        self.warn(path, w);
                    }
                    true
                }
                Err(err) => {
                    self.queue_error(path, Stage::CreateDir, err);
                    false
                }
            },
            Undone::CreatedDir { path, whole, mtime } => {
                let Ok(meta) = std::fs::symlink_metadata(path) else {
                    // Already gone. Nothing to take back, and nothing wrong.
                    return true;
                };
                if !*whole {
                    // Its children had records of their own and have already
                    // gone back, so this should be empty. `remove_dir` refuses
                    // if it is not, which is exactly the check wanted: whatever
                    // is still in there is not this job's to remove.
                    //
                    // Children copied back across a boundary are still here,
                    // waiting on a sync before they are removed (see
                    // `release_source`), so the directory waits behind them.
                    // If it then cannot go, the record is owed again.
                    if !self.pending.is_empty() {
                        self.pending.push(Release::Dir {
                            path: path.clone(),
                            done: None,
                            owed: Some(record.clone()),
                        });
                        return true;
                    }
                    return match std::fs::remove_dir(path) {
                        Ok(()) => true,
                        Err(err) => {
                            self.queue_error(path, Stage::DeleteSource, err);
                            false
                        }
                    };
                }
                // This record stands for the whole subtree, so removing it
                // would take anything added since with it. A directory's mtime
                // moves when an entry is added, removed or renamed.
                if meta.modified().ok() != *mtime {
                    self.queue_error(
                        path,
                        Stage::Write,
                        std::io::Error::other(
                            "this has been added to or emptied since the transfer",
                        ),
                    );
                    return false;
                }
                self.take_back(path, None)
            }
            Undone::Created {
                path,
                from,
                dev,
                ino,
                len,
                mtime,
            } => {
                let Ok(meta) = std::fs::symlink_metadata(path) else {
                    // Already gone. Nothing to take back, and nothing wrong.
                    return true;
                };
                if meta.dev() != *dev || meta.ino() != *ino {
                    self.queue_error(
                        path,
                        Stage::Write,
                        std::io::Error::other("something else is here now"),
                    );
                    return false;
                }
                if meta.len() != *len || meta.modified().ok() != *mtime {
                    self.queue_error(
                        path,
                        Stage::Write,
                        std::io::Error::other("this has been edited since the transfer"),
                    );
                    return false;
                }
                self.take_back(path, from.as_deref())
            }
        }
    }

    /// Whether `path` is still the thing the record was written about.
    fn still_the_same(&mut self, path: &Path, dev: u64, ino: u64) -> bool {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.dev() == dev && meta.ino() == ino => true,
            Ok(_) => {
                self.queue_error(
                    path,
                    Stage::Write,
                    std::io::Error::other("something else is here now"),
                );
                false
            }
            Err(err) => {
                self.queue_error(path, Stage::Walk, err);
                false
            }
        }
    }

    /// Remove something this job's transfer put here, or move it back where it
    /// came from.
    ///
    /// `home` is set for a file whose source was deleted after it landed — a
    /// move across filesystems — and then this is that move in reverse, at the
    /// price it was bought for. Otherwise the source is still where it was and
    /// taking the copy away is the whole of it.
    ///
    /// Taking it away means the trash, not `unlink`. Undo is a guess about
    /// what someone wanted, and a guess that destroys files is one they cannot
    /// take back.
    fn take_back(&mut self, path: &Path, home: Option<&Path>) -> bool {
        if let Some(home) = home {
            match sys::rename_no_replace(path, home) {
                Ok(()) => {
                    self.stats.renames += 1;
                    self.files_copied += 1;
                    return true;
                }
                Err(err) if err.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
                    // What it was: a copy across a boundary and then a delete.
                    // Reversing it is the same again the other way, through
                    // the same ladder, which is why this is a Worker at all.
                    //
                    // Checked first, because unlike the `rename_no_replace`
                    // above, `copy_file_inner` ends in a plain rename and would
                    // clobber. Something new at the original path is somebody
                    // else's file, and putting ours back is not worth taking
                    // theirs.
                    if std::fs::symlink_metadata(home).is_ok() {
                        self.queue_error(
                            home,
                            Stage::Write,
                            std::io::Error::other("something else is here now"),
                        );
                        return false;
                    }
                    let Ok(meta) = std::fs::symlink_metadata(path) else {
                        return true;
                    };
                    // A symlink is not its target. `copy_file_inner` opens the
                    // source, which follows the link, so this used to write the
                    // target's bytes back as a plain file — losing the link, and
                    // copying however many gigabytes it pointed at to do it.
                    if meta.file_type().is_symlink() {
                        return match meta::copy_symlink(path, home) {
                            Ok(()) => {
                                let _ = std::fs::remove_file(path);
                                self.stats.symlinks += 1;
                                self.files_copied += 1;
                                true
                            }
                            Err(err) => {
                                self.queue_error(path, Stage::Symlink, err);
                                false
                            }
                        };
                    }
                    return matches!(
                        self.copy_file_inner(path, home, &meta, true, false),
                        Step::Ok
                    );
                }
                Err(err) => {
                    self.queue_error(path, Stage::Rename, err);
                    return false;
                }
            }
        }

        match self.bin(path) {
            Some(Ok(_)) => {
                self.files_copied += 1;
                true
            }
            Some(Err(err)) => {
                self.queue_error(path, Stage::Rename, err);
                false
            }
            None => {
                // Refused, not deleted. Undo is a guess about what somebody
                // wanted, and the whole reason it moves files to the trash is
                // so a wrong guess costs nothing. Falling back to unlinking
                // where no trash exists made the one case that cannot be taken
                // back the one case with no safety net — and for a directory
                // that meant `remove_dir_all` over a subtree this job had
                // stopped keeping records of.
                self.queue_error(
                    path,
                    Stage::DeleteSource,
                    std::io::Error::other(
                        "nothing here can hold a trash, so this was left rather than deleted",
                    ),
                );
                false
            }
        }
    }
}
