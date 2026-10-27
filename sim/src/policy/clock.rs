//! Clock (second chance): a hand sweeps the frames; a frame with its
//! reference bit set gets the bit cleared and is skipped once.

use super::Policy;
use crate::{FrameIdx, PageId};

pub struct Clock {
    ref_bits: Vec<bool>,
    hand: FrameIdx,
}

impl Clock {
    pub fn new(frames: usize) -> Self {
        Self {
            ref_bits: vec![false; frames],
            hand: 0,
        }
    }
}

impl Policy for Clock {
    fn on_hit(&mut self, frame: FrameIdx, _page: PageId, _pos: usize) {
        self.ref_bits[frame] = true;
    }

    fn on_load(&mut self, frame: FrameIdx, _page: PageId, _pos: usize) {
        self.ref_bits[frame] = true;
    }

    fn victim(&mut self) -> FrameIdx {
        let n = self.ref_bits.len();
        loop {
            let idx = self.hand;
            self.hand = (self.hand + 1) % n;
            if self.ref_bits[idx] {
                self.ref_bits[idx] = false;
            } else {
                return idx;
            }
        }
    }

    fn ref_bit(&self, frame: FrameIdx) -> bool {
        self.ref_bits[frame]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_chance() {
        let mut c = Clock::new(3);
        for f in 0..3 {
            c.on_load(f, f as PageId, f);
        }
        // All bits set: first sweep clears them, then frame 0 is evicted.
        assert_eq!(c.victim(), 0);
        c.on_load(0, 10, 3);
        c.on_hit(1, 1, 4);
        // Frame 1 was re-referenced, frame 2 was not.
        assert_eq!(c.victim(), 2);
    }
}
