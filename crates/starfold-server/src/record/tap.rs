//! Where recordings take the H.264 stream: beside its viewers, never among them. The
//! encode thread hands each frame of the cohort being recorded to every tap as it hands it
//! to the viewers, with the size its encoder was opened at. A tap is not a hub client:
//! flow control, the cohort's display rate, its bitrate and `deliver` never see it.
//!
//! A frame waiting for a recording's disk is the stream's own buffer, shared with the
//! viewers, and counts against the recording's queue until written: at most [`QUEUE`]
//! bytes, or one frame of up to [`LARGEST_FRAME`] into an empty queue. A frame that does
//! not fit ends the recording; it is never queued.
//!
//! The encode thread never waits on a lock here: it reads the tap list through its own
//! copy ([`Seen`]), refreshed with `try_lock` when the list changed. A tap that ends stops
//! counting as demand on the stream at once ([`Taps::any`]), whoever ends it.

use super::End;
use bytes::Bytes;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

/// The most a recording's queue holds while the disk catches up. A keyframe at the
/// default 8 Mbit/s cap is budgeted at about 1 MiB (4x the cap, at 30 fps); deltas are a few
/// tens of KiB.
pub const QUEUE: usize = 4 << 20;
/// The largest frame a recording takes, and only into an empty queue.
pub const LARGEST_FRAME: usize = 8 << 20;
/// What a queued frame costs besides its bytes (its message and the channel's slot).
pub const OVERHEAD: usize = 256;

/// One encoded frame as a recording gets it.
pub struct Tapped {
    /// The size its encoder was opened at: the size of the picture it encodes.
    pub size: (u32, u32),
    pub key: bool,
    pub captured_at: Instant,
    /// The protocol 1 message, header included (Annex-B from byte 16 on).
    pub data: Bytes,
    /// What it counts against the queue until [`Tap::release`]d.
    pub charge: usize,
}

pub enum Msg {
    Frame(Tapped),
    /// The recording ends, as of the given time: frames captured after it are not in it.
    End(End, Instant),
}

/// One recording's place beside the stream.
pub struct Tap {
    id: u64,
    tx: mpsc::Sender<Msg>,
    /// The cohort whose keyframe it started at (0: none yet).
    started: AtomicU64,
    /// Bytes handed over and not yet written.
    queued: AtomicUsize,
    /// Takes no more frames.
    closed: AtomicBool,
    /// Its hub's count of open taps, given back once, when it ends.
    open: Arc<AtomicUsize>,
}

impl Tap {
    /// Ends the recording as of `at`: frames handed over before this are recorded, none
    /// after, and it is no longer demand on the stream. False if it had ended already.
    pub fn end(&self, why: End, at: Instant) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.open.fetch_sub(1, Ordering::AcqRel);
        let _ = self.tx.send(Msg::End(why, at));
        true
    }

    /// A frame's bytes were written (or dropped).
    pub fn release(&self, charge: usize) {
        self.queued.fetch_sub(charge, Ordering::AcqRel);
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Hands over one frame if it fits the queue; else ends the recording. A writer that
    /// is gone ends it too.
    fn admit(&self, f: Tapped) {
        let q = self.queued();
        if q + f.charge > QUEUE && !(q == 0 && f.charge <= LARGEST_FRAME) {
            let why = match f.charge > LARGEST_FRAME {
                true => End::LargeFrame,
                false => End::DiskSlow,
            };
            self.end(why, f.captured_at);
            return;
        }
        let (charge, at) = (f.charge, f.captured_at);
        self.queued.fetch_add(charge, Ordering::AcqRel);
        if self.tx.send(Msg::Frame(f)).is_err() {
            self.release(charge);
            self.end(End::Closed, at);
        }
    }
}

/// An encode thread's copy of the tap list.
#[derive(Default)]
pub struct Seen {
    version: u64,
    list: Arc<Vec<Arc<Tap>>>,
}

/// The taps of a hub, and which cohort they take frames from.
#[derive(Default)]
pub struct Taps {
    /// Taps not yet removed, replaced whole on each change; the encode thread reads it
    /// only with `try_lock`.
    list: Mutex<Arc<Vec<Arc<Tap>>>>,
    /// Counts the changes of `list`.
    version: AtomicU64,
    /// Taps not yet ended.
    open: Arc<AtomicUsize>,
    /// The H.264 cohort a protocol 1 viewer would get (0: none yet).
    cohort: AtomicU64,
    /// Keyframes asked for, and the count a keyframe every open tap took had answered.
    asked: AtomicU64,
    answered: AtomicU64,
    next: AtomicU64,
}

impl Taps {
    /// Whether any recording takes the stream: the video thread keeps the recorded cohort
    /// encoding only while one does.
    pub fn any(&self) -> bool {
        self.open.load(Ordering::Acquire) > 0
    }

    pub fn cohort(&self) -> u64 {
        self.cohort.load(Ordering::Acquire)
    }

    /// The video thread names the cohort recordings take. A new one starts each tap over at
    /// its first keyframe.
    pub fn set_cohort(&self, id: u64) {
        if self.cohort.swap(id, Ordering::AcqRel) != id {
            self.ask_key();
        }
    }

    /// Whether a tap waits for a keyframe from the recorded cohort.
    pub fn wants_key(&self) -> bool {
        self.any() && self.asked.load(Ordering::Acquire) > self.answered.load(Ordering::Acquire)
    }

    /// What follows must start from a keyframe (an encode failed, a tap came, the cohort
    /// changed).
    pub fn ask_key(&self) {
        if self.any() {
            self.asked.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// A new tap; it gets frames from the recorded cohort's next keyframe on.
    pub fn add(&self) -> (Arc<Tap>, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let tap = Arc::new(Tap {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            tx,
            started: AtomicU64::new(0),
            queued: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            open: self.open.clone(),
        });
        self.open.fetch_add(1, Ordering::AcqRel);
        self.change(|v| v.push(tap.clone()));
        self.ask_key();
        (tap, rx)
    }

    /// Ends `tap` if it hadn't (as of now, `why`) and takes it out of the list.
    pub fn remove(&self, tap: &Tap, why: End) {
        tap.end(why, Instant::now());
        self.change(|v| v.retain(|t| t.id != tap.id));
    }

    /// Holds the list's lock, as a start or a stop does for a moment (tests).
    #[cfg(test)]
    pub fn hold_list(&self) -> parking_lot::MutexGuard<'_, Arc<Vec<Arc<Tap>>>> {
        self.list.lock()
    }

    fn change(&self, f: impl FnOnce(&mut Vec<Arc<Tap>>)) {
        let mut list = self.list.lock();
        let mut v = (**list).clone();
        f(&mut v);
        *list = Arc::new(v);
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    /// Called by `cohort`'s encode thread with each frame it hands its viewers. Never waits:
    /// `seen` is its copy of the list, refreshed only when the lock is free. A tap the copy
    /// misses loses nothing: it waits for a keyframe, and a keyframe answers the taps'
    /// request only when the copy was current.
    pub fn offer(
        &self,
        seen: &mut Seen,
        cohort: u64,
        size: (u32, u32),
        all_key: bool,
        captured_at: Instant,
        msgs: &[Bytes],
    ) {
        if cohort != self.cohort() || !self.any() {
            return;
        }
        let asked = self.asked.load(Ordering::Acquire);
        if self.version.load(Ordering::Acquire) != seen.version {
            if let Some(list) = self.list.try_lock() {
                seen.list = list.clone();
                seen.version = self.version.load(Ordering::Acquire);
            }
        }
        for t in seen.list.iter() {
            if t.is_closed() {
                continue;
            }
            if t.started.load(Ordering::Acquire) != cohort {
                if !all_key {
                    continue;
                }
                t.started.store(cohort, Ordering::Release);
            }
            let [data] = msgs else {
                // A recording is one picture per frame; this stream no longer is.
                t.end(End::Closed, captured_at);
                continue;
            };
            t.admit(Tapped {
                size,
                key: all_key,
                captured_at,
                data: data.clone(),
                charge: data.len() + OVERHEAD,
            });
        }
        if all_key && self.version.load(Ordering::Acquire) == seen.version {
            self.answered.fetch_max(asked, Ordering::AcqRel);
        }
    }
}
