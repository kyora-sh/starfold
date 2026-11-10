//! How coarse each macroblock of a stream's picture still is, for the encoders that sharpen a
//! paint-over by QP.

use super::Refine;

/// What one QP step costs per macroblock, until a frame says otherwise: desktop text is
/// 1.3 to 2, a screen packed with it 5 to 15. Starting high costs a thin link one soft pass,
/// starting low a frame that blocks input.
const REFINE_COST: f64 = 5.0;
const REFINE_COST_RANGE: (f64, f64) = (1.0, 15.0);

/// How coarse each macroblock still is, and what sharpening them costs. A QP step costs the
/// same below 28 and less and less above it, down to almost nothing at 51 (1080p text,
/// measured). A fixed QP reaches every macroblock whatever changed, so all of them count.
pub(crate) struct Sharpness {
    pub(crate) qp: Vec<u8>,
    cost: f64,
    pub(crate) off: bool,
}

impl Sharpness {
    pub(crate) fn new(mbs: usize, qp: i32) -> Self {
        Sharpness {
            qp: vec![qp as u8; mbs],
            cost: REFINE_COST,
            off: false,
        }
    }

    /// A frame went out under rate control at `qp`, touching what `static_mbs` doesn't mark.
    pub(crate) fn moved(&mut self, qp: i32, static_mbs: Option<&[u8]>) {
        match static_mbs.filter(|m| m.len() == self.qp.len()) {
            Some(m) => {
                for (q, _) in self.qp.iter_mut().zip(m).filter(|(_, &s)| s == 0) {
                    *q = qp as u8;
                }
            }
            None => self.qp.fill(qp as u8),
        }
    }

    /// Macroblock-steps from where the picture stands to `qp`, in units of `cost`.
    fn steps(&self, qp: i32) -> f64 {
        let mut count = [0usize; 52];
        for &q in &self.qp {
            count[q.min(51) as usize] += 1;
        }
        let (mut total, mut below) = (0.0, 0);
        for q in (qp.max(0)..51).rev() {
            below += count[q as usize + 1];
            total += below as f64 * ((52 - q) as f64 / 24.0).min(1.0);
        }
        total
    }

    /// The lowest QP down to the pass's whose frame is expected to fit.
    pub(crate) fn pick(&self, r: Refine) -> Option<i32> {
        let want = r.qp as i32;
        let top = self.qp.iter().copied().max().unwrap_or(0) as i32;
        if self.off {
            return None;
        }
        match r.max_bytes {
            None => (want < top).then_some(want),
            Some(max) => (want..top).find(|&q| self.steps(q) * self.cost <= max as f64),
        }
    }

    /// A frame larger than expected replaces the estimate at once, a smaller one by half:
    /// passes over the same picture vary several times over, and only too large a frame
    /// hurts. One with next to nothing to sharpen says nothing about the next picture.
    pub(crate) fn refined(&mut self, qp: i32, bytes: usize) {
        let steps = self.steps(qp);
        if steps > 0.0 && bytes >= 1024 {
            let seen = bytes as f64 / steps;
            let cost = if seen > self.cost {
                seen
            } else {
                (self.cost + seen) / 2.0
            };
            self.cost = cost.clamp(REFINE_COST_RANGE.0, REFINE_COST_RANGE.1);
        }
        for q in &mut self.qp {
            *q = (*q).min(qp as u8);
        }
    }

    /// A pass that didn't fit: an estimate left high by an odd frame must not stop every later one.
    pub(crate) fn skipped(&mut self) {
        self.cost = (self.cost * 0.75).max(REFINE_COST.min(self.cost));
    }
}
