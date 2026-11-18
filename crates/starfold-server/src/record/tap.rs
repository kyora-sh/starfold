//! Where recordings take the H.264 stream: beside its viewers, never among them. The
//! encode thread hands each frame of the cohort being recorded to every tap as it hands it
//! to the viewers, with the size its encoder was opened at. A tap is not a hub client:
//! flow control, the cohort's display rate, its bitrate and `deliver` never see it.
//!
//! A frame waiting for a recording's disk is the stream's own buffer, shared with the
//! viewers, and counts against the recording's queue until written: at most [`QUEUE`]
//! bytes, or one frame of up to [`LARGEST_FRAME`] into an empty queue. A frame that does
//! not fit ends the recording; it is never queued.

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
    End(End),
}

/// One recording's place beside the stream.
pub struct Tap {
    id: u64,
    tx: mpsc::Sender<Msg>,
    /// Has had a keyframe from the cohort it records now.
    started: AtomicBool,
    /// Bytes handed over and not yet written.
    queued: AtomicUsize,
    /// Takes no more frames.
    closed: AtomicBool,
}

impl Tap {
    /// Ends the recording: frames handed over before this are recorded, none after.
    pub fn end(&self, why: End) {
        let _ = self.tx.send(Msg::End(why));
    }

    /// A frame's bytes were written (or dropped).
    pub fn release(&self, charge: usize) {
        self.queued.fetch_sub(charge, Ordering::AcqRel);
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Takes no more frames; true the first time.
    pub fn close(&self) -> bool {
        !self.closed.swap(true, Ordering::AcqRel)
    }

    /// Hands over one frame if it fits the queue; else ends the recording.
    fn admit(&self, f: Tapped) {
        let q = self.queued();
        if q + f.charge > QUEUE && !(q == 0 && f.charge <= LARGEST_FRAME) {
            if self.close() {
                self.end(match f.charge > LARGEST_FRAME {
                    true => End::LargeFrame,
                    false => End::DiskSlow,
                });
            }
            return;
        }
        self.queued.fetch_add(f.charge, Ordering::AcqRel);
        let _ = self.tx.send(Msg::Frame(f));
    }
}

/// The taps of a hub, and which cohort they take frames from.
#[derive(Default)]
pub struct Taps {
    list: Mutex<Vec<Arc<Tap>>>,
    /// The H.264 cohort a protocol 1 viewer would get (0: none yet).
    cohort: AtomicU64,
    /// A tap waits for a keyframe.
    key: AtomicBool,
    next: AtomicU64,
}

impl Taps {
    /// Whether any recording runs: the video thread keeps the recorded cohort encoding.
    pub fn any(&self) -> bool {
        !self.list.lock().is_empty()
    }

    pub fn cohort(&self) -> u64 {
        self.cohort.load(Ordering::Acquire)
    }

    /// The video thread names the cohort recordings take. A new one starts each tap over at
    /// its first keyframe.
    pub fn set_cohort(&self, id: u64) {
        let list = self.list.lock();
        if self.cohort.swap(id, Ordering::AcqRel) == id {
            return;
        }
        for t in list.iter() {
            t.started.store(false, Ordering::Release);
        }
        if !list.is_empty() {
            self.key.store(true, Ordering::Release);
        }
    }

    /// Whether a tap waits for a keyframe from the recorded cohort.
    pub fn wants_key(&self) -> bool {
        self.key.load(Ordering::Acquire) && self.any()
    }

    /// An encode failed: what follows must start from a keyframe, as for viewers.
    pub fn ask_key(&self) {
        if self.any() {
            self.key.store(true, Ordering::Release);
        }
    }

    /// A new tap; it gets frames from the recorded cohort's next keyframe on.
    pub fn add(&self) -> (Arc<Tap>, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let tap = Arc::new(Tap {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            tx,
            started: AtomicBool::new(false),
            queued: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        });
        self.list.lock().push(tap.clone());
        self.key.store(true, Ordering::Release);
        (tap, rx)
    }

    pub fn remove(&self, tap: &Tap) {
        self.list.lock().retain(|t| t.id != tap.id);
    }

    /// Called by `cohort`'s encode thread with each frame it hands its viewers. Never waits.
    pub fn offer(
        &self,
        cohort: u64,
        size: (u32, u32),
        all_key: bool,
        captured_at: Instant,
        msgs: &[Bytes],
    ) {
        if cohort != self.cohort() {
            return;
        }
        let list = self.list.lock();
        if list.is_empty() {
            return;
        }
        if all_key {
            self.key.store(false, Ordering::Release);
        }
        for t in list.iter() {
            if t.closed.load(Ordering::Acquire) {
                continue;
            }
            if !t.started.load(Ordering::Acquire) {
                if !all_key {
                    continue;
                }
                t.started.store(true, Ordering::Release);
            }
            let [data] = msgs else {
                // A recording is one picture per frame; this stream no longer is.
                if t.close() {
                    t.end(End::Closed);
                }
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
    }
}
