//! Conflict resolution: sticky policy plus a cancel-aware ask-the-UI wait.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::events::{ConflictChoice, ConflictDecision, Event};

pub struct ConflictState {
    /// `Some` once pre-decided by policy or after an apply-to-all answer.
    sticky: Option<ConflictChoice>,
    events: mpsc::Sender<Event>,
    cancel: Arc<AtomicBool>,
}

/// What the caller should do with the current file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Proceed(ConflictChoice),
    CancelJob,
}

impl ConflictState {
    pub fn new(
        preset: Option<ConflictChoice>,
        events: mpsc::Sender<Event>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sticky: preset,
            events,
            cancel,
        }
    }

    /// Resolve a conflict for `dest`. Blocks *the worker thread only* until the UI
    /// answers, the job is cancelled, or the UI drops the reply sender.
    pub fn resolve(&mut self, src: &Path, dest: &Path) -> Resolution {
        if let Some(choice) = self.sticky {
            return Resolution::Proceed(choice);
        }

        let (tx, rx) = mpsc::channel();
        if self
            .events
            .send(Event::Conflict {
                src: src.to_path_buf(),
                dest: dest.to_path_buf(),
                reply: tx,
            })
            .is_err()
        {
            // The UI dropped the whole event channel: nobody is listening.
            return Resolution::CancelJob;
        }

        loop {
            if self.cancel.load(Ordering::Relaxed) {
                // A late answer from the prompt lands in a dropped receiver — harmless.
                return Resolution::CancelJob;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ConflictDecision::Apply {
                    choice,
                    apply_to_all,
                }) => {
                    if apply_to_all {
                        self.sticky = Some(choice);
                    }
                    return Resolution::Proceed(choice);
                }
                Ok(ConflictDecision::CancelJob) => return Resolution::CancelJob,
                Err(RecvTimeoutError::Timeout) => continue,
                // UI bug dropped the sender: cancel rather than hang forever.
                Err(RecvTimeoutError::Disconnected) => return Resolution::CancelJob,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state(preset: Option<ConflictChoice>) -> (ConflictState, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        (
            ConflictState::new(preset, tx, Arc::new(AtomicBool::new(false))),
            rx,
        )
    }

    #[test]
    fn sticky_policy_answers_without_events() {
        let (mut s, rx) = state(Some(ConflictChoice::Skip));
        let r = s.resolve(Path::new("/a"), Path::new("/b"));
        assert_eq!(r, Resolution::Proceed(ConflictChoice::Skip));
        assert!(rx.try_recv().is_err(), "no event should be emitted");
    }

    #[test]
    fn apply_to_all_becomes_sticky() {
        let (mut s, rx) = state(None);
        let answer = std::thread::spawn(move || {
            let Ok(Event::Conflict { reply, .. }) = rx.recv() else {
                panic!("expected conflict event");
            };
            reply
                .send(ConflictDecision::Apply {
                    choice: ConflictChoice::Overwrite,
                    apply_to_all: true,
                })
                .unwrap();
            rx // keep the channel alive until resolve returns
        });
        let r = s.resolve(Path::new("/a"), Path::new("/b"));
        assert_eq!(r, Resolution::Proceed(ConflictChoice::Overwrite));
        let rx = answer.join().unwrap();

        // Second conflict: sticky, no new event.
        let r = s.resolve(Path::new("/c"), Path::new("/d"));
        assert_eq!(r, Resolution::Proceed(ConflictChoice::Overwrite));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cancel_flag_resolves_a_pending_wait() {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut s = ConflictState::new(None, tx, cancel.clone());

        let flag = cancel.clone();
        let _late = std::thread::spawn(move || {
            let _keep: PathBuf = match rx.recv() {
                Ok(Event::Conflict { dest, .. }) => dest,
                _ => panic!(),
            };
            // Never answer; flip the cancel flag instead (strip-button cancel).
            flag.store(true, Ordering::Relaxed);
            // Hold rx so Disconnected isn't the exit route being tested.
            std::thread::sleep(Duration::from_millis(300));
            drop(rx);
        });

        let r = s.resolve(Path::new("/a"), Path::new("/b"));
        assert_eq!(r, Resolution::CancelJob);
    }

    #[test]
    fn dropped_reply_sender_cancels_instead_of_hanging() {
        let (mut s, rx) = state(None);
        std::thread::spawn(move || {
            let Ok(Event::Conflict { reply, .. }) = rx.recv() else {
                panic!()
            };
            drop(reply); // UI bug
            std::thread::sleep(Duration::from_millis(300));
            drop(rx);
        });
        let r = s.resolve(Path::new("/a"), Path::new("/b"));
        assert_eq!(r, Resolution::CancelJob);
    }
}
