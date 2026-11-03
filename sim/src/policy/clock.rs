//! Clock (second chance): a hand sweeps the frames; a frame with its
//! reference bit set gets the bit cleared and is skipped once.

use super::{Ctx, FrameState, Log, Note, Policy, Why};
use crate::FrameIdx;

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

    fn reference(&mut self, frame: FrameIdx, log: &mut Log) {
        if !self.ref_bits[frame] {
            self.ref_bits[frame] = true;
            log.push(Note::Counted {
                frame,
                from: 0,
                to: 1,
            });
        }
    }
}

impl Policy for Clock {
    fn on_hit(&mut self, frame: FrameIdx, _ctx: Ctx, log: &mut Log) {
        self.reference(frame, log);
    }

    fn on_load(&mut self, frame: FrameIdx, _ctx: Ctx, log: &mut Log) {
        self.reference(frame, log);
    }

    fn victim(&mut self, _ctx: Ctx, log: &mut Log) -> FrameIdx {
        let n = self.ref_bits.len();
        loop {
            let frame = self.hand;
            self.hand = (self.hand + 1) % n;
            if self.ref_bits[frame] {
                self.ref_bits[frame] = false;
                log.push(Note::Swept {
                    frame,
                    from: 1,
                    to: 0,
                });
            } else {
                log.push(Note::Evict {
                    frame,
                    why: Why::CountZero,
                });
                return frame;
            }
        }
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        FrameState::Clock {
            referenced: self.ref_bits[frame],
        }
    }

    fn hand(&self) -> Option<FrameIdx> {
        Some(self.hand)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::test_util::{at, log};

    #[test]
    fn second_chance() {
        let mut c = Clock::new(3);
        let mut l = log();
        for f in 0..3 {
            c.on_load(f, at(f, 0), &mut l);
        }
        l.take();
        // All bits set: first sweep clears them, then frame 0 is evicted.
        assert_eq!(c.victim(at(3, 0), &mut l), 0);
        let swept = |frame| Note::Swept {
            frame,
            from: 1,
            to: 0,
        };
        let evict = |frame| Note::Evict {
            frame,
            why: Why::CountZero,
        };
        assert_eq!(l.take(), [swept(0), swept(1), swept(2), evict(0)]);
        c.on_load(0, at(3, 0), &mut l);
        c.on_hit(1, at(4, 0), &mut l);
        // Frame 1 was re-referenced, frame 2 was not.
        assert_eq!(c.victim(at(5, 0), &mut l), 2);
        assert_eq!(c.hand(), Some(0));
        assert_eq!(c.state(1), FrameState::Clock { referenced: false });
    }
}
