//! FIFO: evict the page that was loaded longest ago, ignoring hits.
//! Suffers from Belady's anomaly (more frames can mean more misses).

use std::collections::VecDeque;

use super::{Ctx, FrameState, Log, Note, Policy, Why};
use crate::FrameIdx;

pub struct Fifo {
    queue: VecDeque<FrameIdx>,
    loaded_at: Vec<usize>,
}

impl Fifo {
    pub fn new(frames: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(frames),
            loaded_at: vec![0; frames],
        }
    }
}

impl Policy for Fifo {
    fn on_hit(&mut self, _frame: FrameIdx, _ctx: Ctx, _log: &mut Log) {}

    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.queue.push_back(frame);
        self.loaded_at[frame] = ctx.pos;
    }

    fn victim(&mut self, _ctx: Ctx, log: &mut Log) -> FrameIdx {
        let frame = self
            .queue
            .pop_front()
            .expect("victim called with no resident pages");
        let why = Why::LoadedFirst {
            loaded_at: self.loaded_at[frame],
        };
        log.push(Note::Evict { frame, why });
        frame
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        let rank = self.queue.iter().position(|&f| f == frame);
        FrameState::Fifo {
            rank: rank.expect("state of a resident frame"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::test_util::{at, log};

    #[test]
    fn evicts_in_load_order_regardless_of_hits() {
        let mut p = Fifo::new(3);
        let mut l = log();
        for f in 0..3 {
            p.on_load(f, at(f, 0), &mut l);
        }
        p.on_hit(0, at(3, 0), &mut l);
        assert_eq!(p.state(1), FrameState::Fifo { rank: 1 });
        assert_eq!(p.victim(at(4, 0), &mut l), 0);
        p.on_load(0, at(4, 0), &mut l);
        assert_eq!(p.victim(at(5, 0), &mut l), 1);
        assert_eq!(
            l.take()[1],
            Note::Evict {
                frame: 1,
                why: Why::LoadedFirst { loaded_at: 1 }
            }
        );
    }
}
