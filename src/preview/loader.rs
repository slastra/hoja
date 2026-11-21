//! The previews hoja has made, and the work of making more.
//!
//! Whatever shows previews says what it wants, as a whole set at a time:
//! `want(owner, [(key, need, priority)])` replaces everything `owner` asked
//! for before. The hover tooltip wants one file. The grid wants the cards in
//! view and a margin round them, with their pictures, and the cards a few
//! screens further on for their facts alone, so their shapes are known before
//! they come into view.
//!
//! The union of every owner's set does two jobs:
//!
//! - **It is the queue.** `pump` starts the best-placed wanted work while
//!   fewer than `FACTS_WORKERS` facts jobs and `PICTURE_WORKERS` thumbnail
//!   jobs are running. Work nobody wants any more is never started. Work
//!   already running finishes and is kept: it was paid for, and the pointer
//!   often comes back.
//! - **It pins.** A preview that something wants is never evicted, so the
//!   cards on screen keep their textures however many others are made.
//!
//! An owner is told when a key it wants learns its facts or gets its picture,
//! by `cx.notify` on the owner's entity, and reads the answer with `known`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gpui::{App, AppContext as _, EntityId, Global, RenderImage};

use super::thumbnail::Size;
use super::{Facts, Preview, Subject};

/// Facts jobs at once. Each is a header read, a MIME sniff, and for a PDF a
/// `pdfinfo` run of tens of milliseconds.
const FACTS_WORKERS: usize = 4;
/// Thumbnail jobs at once. A thumbnailer is a process that decodes video, so
/// fewer of these than there are cores.
const PICTURE_WORKERS: usize = 3;
/// Finished previews kept, and so pictures kept in each window's atlas, not
/// counting the ones something wants, which are never evicted.
const KEEP: usize = 400;
/// Facts kept for files whose picture is not wanted yet. Small values: a few
/// numbers, a text excerpt of at most 16 short lines.
const KEEP_FACTS: usize = 4096;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub subject: Subject,
    pub size: Size,
}

/// How much of a preview is wanted.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Need {
    /// The kind and the shape: what a layout needs to place a file.
    Facts,
    /// The picture as well.
    Picture,
}

/// What is known about a key now.
pub enum Known {
    Nothing,
    /// The facts, with the picture still to come if there is one.
    Facts(Arc<Facts>),
    /// Everything there will be.
    Done(Arc<Preview>),
}

impl Known {
    pub fn facts(&self) -> Option<&Arc<Facts>> {
        match self {
            Known::Nothing => None,
            Known::Facts(facts) => Some(facts),
            Known::Done(preview) => Some(&preview.facts),
        }
    }

    pub fn done(&self) -> Option<&Arc<Preview>> {
        match self {
            Known::Done(preview) => Some(preview),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Loader {
    /// Finished: facts and picture, or facts of a file with no picture.
    ready: HashMap<Key, Arc<Preview>>,
    /// Oldest first.
    ready_order: VecDeque<Key>,
    /// Facts of files whose picture is not made yet.
    facts: HashMap<Key, Arc<Facts>>,
    facts_order: VecDeque<Key>,
    running_facts: HashSet<Key>,
    running_pictures: HashSet<Key>,
    wants: HashMap<EntityId, Vec<(Key, Need, u32)>>,
    /// Each owner's background sweep: keys whose facts are asked for in
    /// order when no wanted work is waiting, and how far it has got.
    sweeps: HashMap<EntityId, Sweep>,
    /// The settled shape of every key whose shape is known: `Some` aspect,
    /// or `None` for a file with no picture. Kept when the preview is
    /// evicted, which is the point: a few bytes a file, and a card whose
    /// preview was let go keeps its shape, so the cards under it do not move.
    shapes: HashMap<Key, Option<f32>>,
    /// The keys that learned something, oldest first, so a layout can look
    /// at only those rather than ask about every card it has. `log_end` is
    /// the sequence number after the last entry. Trimmed from the front: a
    /// reader that falls further behind than the log reaches starts over.
    log: VecDeque<Key>,
    log_end: u64,
}

/// How many changes the log keeps.
const LOG: usize = 1 << 16;

struct Sweep {
    keys: Arc<Vec<Key>>,
    next: usize,
}

impl Global for Loader {}

impl Loader {
    fn known(&self, key: &Key) -> Known {
        if let Some(preview) = self.ready.get(key) {
            Known::Done(preview.clone())
        } else if let Some(facts) = self.facts.get(key) {
            Known::Facts(facts.clone())
        } else {
            Known::Nothing
        }
    }

    /// Every owner's wants in one, by key: the most any owner needs, at the
    /// best priority any owner gave.
    fn wanted(&self) -> HashMap<&Key, (Need, u32)> {
        let mut wanted: HashMap<&Key, (Need, u32)> = HashMap::new();
        for (key, need, priority) in self.wants.values().flatten() {
            let slot = wanted.entry(key).or_insert((*need, *priority));
            slot.0 = slot.0.max(*need);
            slot.1 = slot.1.min(*priority);
        }
        wanted
    }

    /// The shape of `key` if it is known: from what is cached, else from
    /// the shape memory. The outer `None` is "not known yet".
    fn shape(&self, key: &Key) -> Option<Option<f32>> {
        match self.known(key) {
            Known::Done(preview) => Some(super::settled_shape(&preview)),
            Known::Facts(facts) => Some(facts.aspect),
            Known::Nothing => self.shapes.get(key).copied(),
        }
    }

    /// Whether anything is known or under way for `key`, so a sweep can pass
    /// it by.
    fn settled_or_busy(&self, key: &Key) -> bool {
        self.ready.contains_key(key)
            || self.facts.contains_key(key)
            || self.shapes.contains_key(key)
            || self.running_facts.contains(key)
    }

    /// The work to start now, best placed first, within the worker limits:
    /// facts jobs, then thumbnail jobs. Facts slots left over go to the
    /// sweeps, which move on past what they hand out.
    fn next(&mut self) -> (Vec<Key>, Vec<Key>) {
        let (mut facts, pictures) = self.next_wanted();
        let mut slots = FACTS_WORKERS
            .saturating_sub(self.running_facts.len())
            .saturating_sub(facts.len());
        let owners: Vec<EntityId> = self.sweeps.keys().copied().collect();
        for owner in owners {
            while slots > 0 {
                let Some(sweep) = self.sweeps.get(&owner) else {
                    break;
                };
                let Some(key) = sweep.keys.get(sweep.next).cloned() else {
                    break;
                };
                self.sweeps.get_mut(&owner).expect("present").next += 1;
                if self.settled_or_busy(&key) || facts.contains(&key) {
                    continue;
                }
                facts.push(key);
                slots -= 1;
            }
        }
        (facts, pictures)
    }

    fn next_wanted(&self) -> (Vec<Key>, Vec<Key>) {
        let mut wanted: Vec<(&Key, (Need, u32))> = self.wanted().into_iter().collect();
        wanted.sort_by_key(|(_, (_, priority))| *priority);
        let mut facts_slots = FACTS_WORKERS.saturating_sub(self.running_facts.len());
        let mut picture_slots = PICTURE_WORKERS.saturating_sub(self.running_pictures.len());
        let (mut facts, mut pictures) = (Vec::new(), Vec::new());
        for (key, (need, _)) in wanted {
            if facts_slots == 0 && picture_slots == 0 {
                break;
            }
            if self.ready.contains_key(key)
                || self.running_facts.contains(key)
                || self.running_pictures.contains(key)
            {
                continue;
            }
            match self.facts.get(key) {
                None if facts_slots > 0 => {
                    facts.push(key.clone());
                    facts_slots -= 1;
                }
                // A file whose facts say it has a picture to make. One without
                // goes straight to `ready` when its facts arrive.
                Some(known)
                    if need == Need::Picture && known.aspect.is_some() && picture_slots > 0 =>
                {
                    pictures.push(key.clone());
                    picture_slots -= 1;
                }
                _ => {}
            }
        }
        (facts, pictures)
    }

    fn remember_shape(&mut self, key: &Key, shape: Option<f32>) {
        self.shapes.insert(key.clone(), shape);
    }

    fn log(&mut self, key: &Key) {
        self.log.push_back(key.clone());
        self.log_end += 1;
        if self.log.len() > LOG {
            self.log.drain(..LOG / 2);
        }
    }

    /// The keys logged from sequence number `since` on, and where the log
    /// ends now; `None` when `since` is further back than the log reaches.
    fn changes_since(&self, since: u64) -> Option<(Vec<Key>, u64)> {
        let start = self.log_end - self.log.len() as u64;
        let from = since.checked_sub(start)?;
        let keys = self.log.iter().skip(from as usize).cloned().collect();
        Some((keys, self.log_end))
    }

    fn pinned(&self) -> HashSet<Key> {
        self.wants
            .values()
            .flatten()
            .map(|(key, _, _)| key.clone())
            .collect()
    }

    /// Keep facts for a file whose picture is still to come.
    fn keep_facts(&mut self, key: Key, facts: Arc<Facts>) {
        if self.facts.insert(key.clone(), facts).is_some() {
            self.facts_order.retain(|k| k != &key);
        }
        self.facts_order.push_back(key);
        let pinned = self.pinned();
        evict(&mut self.facts, &mut self.facts_order, KEEP_FACTS, &pinned);
    }

    /// Keep a finished preview, and give back the pictures of what it pushed
    /// out: an older preview of the same key, and the oldest unpinned ones
    /// past `KEEP`. Their textures are the caller's to drop.
    fn keep_ready(&mut self, key: Key, preview: Arc<Preview>) -> Vec<Arc<RenderImage>> {
        self.facts.remove(&key);
        self.facts_order.retain(|k| k != &key);
        let mut gone = Vec::new();
        if let Some(old) = self.ready.insert(key.clone(), preview) {
            self.ready_order.retain(|k| k != &key);
            gone.push(old);
        }
        self.ready_order.push_back(key);
        let pinned = self.pinned();
        gone.extend(evict(&mut self.ready, &mut self.ready_order, KEEP, &pinned));
        gone.iter()
            .filter_map(|preview| Some(preview.picture.as_ref()?.image.clone()))
            .collect()
    }

    /// The owners that want `key`, to be told it changed.
    fn watchers(&self, key: &Key) -> Vec<EntityId> {
        self.wants
            .iter()
            .filter(|(_, wants)| wants.iter().any(|(k, _, _)| k == key))
            .map(|(owner, _)| *owner)
            .collect()
    }
}

/// Remove the oldest unpinned entries until `map` is down to `keep`, or only
/// pinned ones are left. Returns what was removed.
fn evict<V>(
    map: &mut HashMap<Key, V>,
    order: &mut VecDeque<Key>,
    keep: usize,
    pinned: &HashSet<Key>,
) -> Vec<V> {
    let mut gone = Vec::new();
    let mut at = 0;
    while map.len() > keep && at < order.len() {
        if pinned.contains(&order[at]) {
            at += 1;
            continue;
        }
        let key = order.remove(at).expect("index is in range");
        gone.extend(map.remove(&key));
    }
    gone
}

/// Replace everything `owner` wants with `wants`, and start what is now due.
/// Cheap when nothing changed, so a caller can say it on every frame.
pub fn want(owner: EntityId, wants: Vec<(Key, Need, u32)>, cx: &mut App) {
    let loader = cx.default_global::<Loader>();
    if loader.wants.get(&owner) == Some(&wants) {
        return;
    }
    loader.wants.insert(owner, wants);
    pump(cx);
}

/// `owner` wants nothing any more: it was released, or stopped showing
/// previews. Its sweep stops too.
pub fn forget(owner: EntityId, cx: &mut App) {
    let loader = cx.default_global::<Loader>();
    let swept = loader.sweeps.remove(&owner).is_some();
    if loader.wants.remove(&owner).is_some() || swept {
        pump(cx);
    }
}

/// Replace `owner`'s sweep with `keys`, from the first: their facts are asked
/// for in the background, in that order, behind everything any owner wants on
/// screen. A grid gives its whole listing, so the shapes of cards it has not
/// reached are known before it reaches them.
pub fn sweep(owner: EntityId, keys: Arc<Vec<Key>>, cx: &mut App) {
    let loader = cx.default_global::<Loader>();
    if loader
        .sweeps
        .get(&owner)
        .is_some_and(|sweep| Arc::ptr_eq(&sweep.keys, &keys))
    {
        return;
    }
    loader.sweeps.insert(owner, Sweep { keys, next: 0 });
    pump(cx);
}

/// The keys that learned something since sequence number `since`, and the
/// number to ask from next time; `None` when the log no longer reaches back
/// that far and everything has to be looked at again.
pub fn changes_since(since: u64, cx: &App) -> Option<(Vec<Key>, u64)> {
    match cx.try_global::<Loader>() {
        Some(loader) => loader.changes_since(since),
        None => Some((Vec::new(), since)),
    }
}

/// Where the change log ends now: what a new reader asks from. It moves
/// whenever anything is learned, so it also tells a layout made from what was
/// known that it may be out of date.
pub fn log_end(cx: &App) -> u64 {
    cx.try_global::<Loader>().map_or(0, |loader| loader.log_end)
}

/// The shape of `key` if it is known. See `Loader::shape`.
pub fn shape(key: &Key, cx: &App) -> Option<Option<f32>> {
    cx.try_global::<Loader>()?.shape(key)
}

pub fn known(key: &Key, cx: &App) -> Known {
    cx.try_global::<Loader>()
        .map_or(Known::Nothing, |loader| loader.known(key))
}

/// Start whatever is due.
fn pump(cx: &mut App) {
    let loader = cx.default_global::<Loader>();
    let (facts, pictures) = loader.next();
    loader.running_facts.extend(facts.iter().cloned());
    loader.running_pictures.extend(pictures.iter().cloned());
    for key in facts {
        cx.spawn(async move |cx| {
            let path = key.subject.path.clone();
            let facts = cx
                .background_spawn(async move { Arc::new(super::facts(&path)) })
                .await;
            cx.update(|cx| learned(key, facts, cx));
        })
        .detach();
    }
    for key in pictures {
        let Some(facts) = cx.default_global::<Loader>().facts.get(&key).cloned() else {
            cx.default_global::<Loader>().running_pictures.remove(&key);
            continue;
        };
        cx.spawn(async move |cx| {
            let (path, size) = (key.subject.path.clone(), key.size);
            let preview = cx
                .background_spawn(async move { super::picture(&path, facts, size) })
                .await;
            cx.update(|cx| made(key, Arc::new(preview), cx));
        })
        .detach();
    }
}

fn learned(key: Key, facts: Arc<Facts>, cx: &mut App) {
    let loader = cx.default_global::<Loader>();
    loader.running_facts.remove(&key);
    // A file with no picture, or a measured shape, is settled now. A guessed
    // one waits for the picture.
    if facts.aspect.is_none() || facts.measured {
        loader.remember_shape(&key, facts.aspect);
    }
    loader.log(&key);
    let watchers = loader.watchers(&key);
    let dropped = if facts.aspect.is_some() {
        loader.keep_facts(key, facts);
        Vec::new()
    } else {
        // Nothing to draw: the facts are the whole preview.
        let preview = Preview {
            facts,
            picture: None,
            original: None,
        };
        loader.keep_ready(key, Arc::new(preview))
    };
    settle(dropped, watchers, cx);
}

fn made(key: Key, preview: Arc<Preview>, cx: &mut App) {
    let loader = cx.default_global::<Loader>();
    loader.running_pictures.remove(&key);
    loader.remember_shape(&key, super::settled_shape(&preview));
    loader.log(&key);
    let watchers = loader.watchers(&key);
    let dropped = loader.keep_ready(key, preview);
    settle(dropped, watchers, cx);
}

/// Drop the textures that fell out, tell the watchers, and start what is due
/// now that a worker is free.
fn settle(dropped: Vec<Arc<RenderImage>>, watchers: Vec<EntityId>, cx: &mut App) {
    for image in dropped {
        cx.drop_image(image, None);
    }
    for owner in watchers {
        cx.notify(owner);
    }
    pump(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::Kind;
    use std::path::Path;

    fn key(n: u64) -> Key {
        Key {
            subject: Subject {
                path: Path::new("/f").join(n.to_string()).into(),
                modified: None,
                size: Some(n),
            },
            size: Size::Large,
        }
    }

    fn facts(aspect: Option<f32>) -> Arc<Facts> {
        Arc::new(Facts {
            kind: if aspect.is_some() {
                Kind::Image
            } else {
                Kind::Text
            },
            mimes: Vec::new(),
            dimensions: None,
            aspect,
            measured: aspect.is_some(),
            duration: None,
            pages: None,
            lines: None,
            highlights: None,
            tags: None,
        })
    }

    fn done(n: u64) -> (Key, Arc<Preview>) {
        let preview = Preview {
            facts: facts(None),
            picture: None,
            original: None,
        };
        (key(n), Arc::new(preview))
    }

    fn owner(n: u64) -> EntityId {
        EntityId::from(n)
    }

    #[test]
    fn a_sweep_fills_what_the_screen_leaves_and_skips_what_is_known() {
        let mut loader = Loader::default();
        // One card on screen, and a sweep of ten in which two are known.
        loader
            .wants
            .insert(owner(1), vec![(key(100), Need::Picture, 0)]);
        loader.shapes.insert(key(1), Some(1.));
        loader.keep_facts(key(2), facts(Some(1.)));
        loader.sweeps.insert(
            owner(1),
            Sweep {
                keys: Arc::new((0..10).map(key).collect()),
                next: 0,
            },
        );
        let (facts, _) = loader.next();
        // The screen first, then the sweep in order, past the known ones.
        assert_eq!(facts, [key(100), key(0), key(3), key(4)]);
        // It moves on, so the next call carries on from there. (The card on
        // screen is due again: its facts never arrived.)
        let (facts, _) = loader.next();
        assert_eq!(facts[..3], [key(100), key(5), key(6)]);
    }

    #[test]
    fn a_shape_outlives_its_evicted_preview() {
        let mut loader = Loader::default();
        let (k, _) = done(0);
        loader.remember_shape(&k, Some(1.5));
        for n in 0..KEEP as u64 + 5 {
            let (k, p) = done(n);
            loader.keep_ready(k, p);
        }
        assert!(
            matches!(loader.known(&key(0)), Known::Nothing),
            "the preview is gone"
        );
        assert_eq!(loader.shape(&key(0)), Some(Some(1.5)), "the shape is not");
        assert_eq!(
            loader.shape(&key(9999)),
            None,
            "and an unknown one is unknown"
        );
    }

    #[test]
    fn work_starts_best_first_within_the_limits() {
        let mut loader = Loader::default();
        let wants = (0..10)
            .map(|n| (key(n), Need::Picture, 10 - n as u32))
            .collect();
        loader.wants.insert(owner(1), wants);
        let (facts, pictures) = loader.next();
        // Facts first, since nothing is known yet, and the best placed:
        // priority 1 is key 9.
        assert_eq!(facts, [key(9), key(8), key(7), key(6)]);
        assert!(pictures.is_empty());
    }

    #[test]
    fn a_picture_waits_for_facts_and_needs_a_shape() {
        let mut loader = Loader::default();
        loader.wants.insert(
            owner(1),
            vec![
                (key(0), Need::Picture, 0),
                (key(1), Need::Facts, 1),
                (key(2), Need::Picture, 2),
            ],
        );
        loader.facts.insert(key(0), facts(Some(1.5)));
        loader.facts.insert(key(1), facts(Some(1.5)));
        loader.facts.insert(key(2), facts(None));
        let (facts, pictures) = loader.next();
        assert!(facts.is_empty());
        // Key 1 is wanted for its facts alone, and key 2 has no picture.
        assert_eq!(pictures, [key(0)]);
    }

    #[test]
    fn running_and_ready_work_is_not_started_again() {
        let mut loader = Loader::default();
        loader.wants.insert(
            owner(1),
            vec![(key(0), Need::Picture, 0), (key(1), Need::Picture, 1)],
        );
        loader.running_facts.insert(key(0));
        let (k, p) = done(1);
        loader.keep_ready(k, p);
        let (facts, pictures) = loader.next();
        assert!(facts.is_empty() && pictures.is_empty());
    }

    #[test]
    fn owners_are_united_at_the_most_need_and_the_best_priority() {
        let mut loader = Loader::default();
        loader
            .wants
            .insert(owner(1), vec![(key(0), Need::Facts, 5)]);
        loader
            .wants
            .insert(owner(2), vec![(key(0), Need::Picture, 50)]);
        let wanted = loader.wanted();
        assert_eq!(wanted[&key(0)], (Need::Picture, 5));
    }

    #[test]
    fn the_cache_keeps_what_is_wanted_and_the_newest() {
        let mut loader = Loader::default();
        // Key 0 is on screen.
        loader
            .wants
            .insert(owner(1), vec![(key(0), Need::Picture, 0)]);
        for n in 0..KEEP as u64 + 2 {
            let (k, p) = done(n);
            loader.keep_ready(k, p);
        }
        assert_eq!(loader.ready.len(), KEEP);
        assert!(loader.ready.contains_key(&key(0)), "pinned");
        assert!(!loader.ready.contains_key(&key(1)));
        assert!(!loader.ready.contains_key(&key(2)));
        assert!(loader.ready.contains_key(&key(3)));

        // The same key again replaces its entry and becomes the newest.
        let (k, p) = done(3);
        loader.keep_ready(k.clone(), p);
        assert_eq!(loader.ready_order.len(), KEEP);
        assert!(loader.ready_order.back() == Some(&k));
    }

    #[test]
    fn a_finished_preview_replaces_its_facts() {
        let mut loader = Loader::default();
        loader.keep_facts(key(0), facts(Some(1.)));
        assert!(matches!(loader.known(&key(0)), Known::Facts(_)));
        let (k, p) = done(0);
        loader.keep_ready(k, p);
        assert!(matches!(loader.known(&key(0)), Known::Done(_)));
        assert!(loader.facts.is_empty() && loader.facts_order.is_empty());
    }

    #[test]
    fn watchers_are_the_owners_that_want_the_key() {
        let mut loader = Loader::default();
        loader
            .wants
            .insert(owner(1), vec![(key(0), Need::Facts, 0)]);
        loader
            .wants
            .insert(owner(2), vec![(key(1), Need::Facts, 0)]);
        assert_eq!(loader.watchers(&key(0)), [owner(1)]);
    }
}
