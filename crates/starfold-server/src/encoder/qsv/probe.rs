//! The Quick Sync probe as the server runs it: in a helper process like any session's
//! (on Windows inside the same kill-on-close job), never in the server. A server that can't
//! start helpers, such as a program that embeds `starfold::run` without `starfold::main`,
//! gets no Quick Sync rather than a probe of its own.
//!
//! What it verified is kept: sessions open on the adapter that passed, not merely the
//! first one listed.

use super::proc::{NotStarted, Pipes, Proc};
use super::remote::Policy;
use crate::encoder::caps::{Probe as Found, ProbeProvider, ProbeStatus};
use crate::encoder::{isolate, Backend, Codec};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// More than any probe prints.
const MAX_OUTPUT: usize = 1 << 20;
const POLL: Duration = Duration::from_millis(5);

/// Starts a probe helper.
pub type Spawn = Box<dyn Fn() -> Result<(Proc, Pipes), NotStarted> + Send>;

/// The adapters the probe verified each codec on, in the order it listed them.
pub struct Verified(Mutex<Vec<(Codec, u64)>>);

impl Verified {
    pub const fn new() -> Verified {
        Verified(Mutex::new(Vec::new()))
    }

    /// The first adapter `codec` was verified on.
    pub fn adapter(&self, codec: Codec) -> Option<u64> {
        let v = self.0.lock().unwrap_or_else(|e| e.into_inner());
        v.iter().find(|(c, _)| *c == codec).map(|&(_, l)| l)
    }

    fn set(&self, found: &Found) {
        let v = found
            .caps
            .iter()
            .filter(|c| c.status == ProbeStatus::Verified)
            .filter_map(|c| Some((c.codec, luid_of(&c.device_id)?)))
            .collect();
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = v;
    }
}

impl Default for Verified {
    fn default() -> Verified {
        Verified::new()
    }
}

/// This process's verified adapters.
pub static VERIFIED: Verified = Verified::new();

/// The LUID a probed device names: `luid:<16 hex digits> ...` (see `Adapter::label`).
pub fn luid_of(device: &str) -> Option<u64> {
    let hex = device.strip_prefix("luid:")?.get(..16)?;
    u64::from_str_radix(hex, 16).ok()
}

/// Probes Quick Sync in a helper.
pub struct Probe {
    /// Whether this executable starts helpers at all.
    pub helpers: bool,
    pub spawn: Spawn,
    pub policy: &'static Policy,
    pub verified: &'static Verified,
    pub timeout: Duration,
}

impl ProbeProvider for Probe {
    fn backend(&self) -> Backend {
        Backend::Qsv
    }

    fn probe(&self) -> Found {
        if !self.helpers {
            return Found::failed(
                Backend::Qsv,
                "needs the helper process: this program doesn't run starfold's helpers",
            );
        }
        let found = self.run();
        self.verified.set(&found);
        found
    }

    fn bounded(&self) -> bool {
        true
    }
}

impl Probe {
    fn run(&self) -> Found {
        let (proc, pipes) = match (self.spawn)() {
            Ok(p) => p,
            Err(n) => {
                let stuck = !n.left.is_empty();
                self.policy.keep(n.left);
                if stuck {
                    self.policy.give_up(&format!(
                        "a probe helper that failed to start did not exit ({})",
                        n.why
                    ));
                }
                return Found::failed(Backend::Qsv, format!("probe didn't start: {}", n.why));
            }
        };
        let proc = Arc::new(proc);
        let Pipes { tx, mut rx } = pipes;
        drop(tx);
        let deadline = Instant::now() + self.timeout;
        let mut out = Vec::new();
        let mut chunk = [0u8; 1 << 14];
        let late = loop {
            match rx.read_ready(&mut chunk) {
                Ok(Some(0)) => break None,
                Ok(Some(n)) if out.len() + n > MAX_OUTPUT => {
                    break Some(format!("probe printed more than {} KiB", MAX_OUTPUT >> 10))
                }
                Ok(Some(n)) => out.extend_from_slice(&chunk[..n]),
                Ok(None) if Instant::now() >= deadline => {
                    break Some(format!(
                        "probe did not finish in {} s and was killed",
                        self.timeout.as_secs()
                    ))
                }
                Ok(None) => std::thread::sleep(POLL),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break Some(format!("probe output: {e}")),
            }
        };
        drop(rx);
        if let Some(e) = late {
            self.policy.end(&proc);
            return Found::failed(Backend::Qsv, e);
        }
        let exit = Instant::now() + self.policy.timing().grace;
        while !proc.exited() && Instant::now() < exit {
            std::thread::sleep(POLL);
        }
        if !proc.exited() {
            self.policy.end(&proc);
            return Found::failed(Backend::Qsv, "probe closed its output but did not exit");
        }
        isolate::parse(&out, Backend::Qsv)
    }
}
