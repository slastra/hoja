//! Attempt-failure caching and the M3 escalation seam.
//!
//! The principle throughout the engine is attempt-and-fallback: try the cheap
//! syscall, catch the errno, and *cache the observed failure* — never predict
//! kernel behavior from st_dev/fs-type sniffing. The first file of a job probes;
//! every later file on the same mount pair skips the doomed attempt.

use std::collections::HashSet;

use crate::sys::MountKey;

#[derive(Debug, Default)]
pub struct MountPairCache {
    rename_failed: HashSet<(MountKey, MountKey)>,
    reflink_failed: HashSet<(MountKey, MountKey)>,
}

impl MountPairCache {
    pub fn rename_worth_trying(&self, src: MountKey, dst: MountKey) -> bool {
        !self.rename_failed.contains(&(src, dst))
    }

    pub fn mark_rename_failed(&mut self, src: MountKey, dst: MountKey) {
        self.rename_failed.insert((src, dst));
    }

    pub fn reflink_worth_trying(&self, src: MountKey, dst: MountKey) -> bool {
        !self.reflink_failed.contains(&(src, dst))
    }

    pub fn mark_reflink_failed(&mut self, src: MountKey, dst: MountKey) {
        self.reflink_failed.insert((src, dst));
    }
}

/// Seam for M3: the serial worker implements this today; a work-stealing pool
/// implements it later without changing `spawn_job`'s signature. `escalate` is
/// invoked when the gate counters (accumulated during the one walk) cross the
/// bulk thresholds; the M1 engine ignores it.
#[allow(dead_code)] // M3 seam: the pool engine implements this; serial ignores it
pub trait Engine {
    fn escalate(&mut self) {}
}

#[allow(dead_code)] // constructed once the seam carries real dispatch in M3
pub struct SerialEngine;

impl Engine for SerialEngine {}
