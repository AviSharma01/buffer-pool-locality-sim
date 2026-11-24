//! OPT (Belady's MIN): evict the page whose next use is furthest in the
//! future. Needs the whole trace, so it is a yardstick, not a real policy:
//! no policy can have fewer misses on the same trace.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use super::{Ctx, FrameState, Log, Note, Policy, Why};
use crate::FrameIdx;

pub struct Opt {
    /// From [`crate::trace::Trace::next_use`].
    next_use: Vec<usize>,
    /// Next-use position of the page in each frame.
    frame_next: Vec<usize>,
    /// Every frame, ordered so the last is the victim: furthest next use,
    /// ties (e.g. several never used again) to the lowest frame.
    by_next: BTreeSet<(usize, Reverse<FrameIdx>)>,
}

impl Opt {
    pub fn new(frames: usize, next_use: Vec<usize>) -> Self {
        Self {
            next_use,
            frame_next: vec![usize::MAX; frames],
            by_next: (0..frames).map(|f| (usize::MAX, Reverse(f))).collect(),
        }
    }

    fn used(&mut self, frame: FrameIdx, ctx: Ctx) {
        let next = self.next_use[ctx.pos];
        self.by_next
            .remove(&(self.frame_next[frame], Reverse(frame)));
        self.by_next.insert((next, Reverse(frame)));
        self.frame_next[frame] = next;
    }
}

fn never_as_none(pos: usize) -> Option<usize> {
    (pos != usize::MAX).then_some(pos)
}

impl Policy for Opt {
    /// Optimal is the bound on misses for the pages queries ask for; it does
    /// not read ahead (a pool that prefetches can beat it on misses).
    fn reads_ahead(&self, _ctx: Ctx, _continues: bool) -> bool {
        false
    }

    fn on_hit(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.used(frame, ctx);
    }

    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.used(frame, ctx);
    }

    fn victim(&mut self, _ctx: Ctx, pinned: Option<FrameIdx>, log: &mut Log) -> FrameIdx {
        let &(next, Reverse(frame)) = self
            .by_next
            .iter()
            .rev()
            .find(|&&(_, Reverse(f))| Some(f) != pinned)
            .expect("pool has frames");
        log.push(Note::Evict {
            frame,
            why: Why::FurthestNextUse {
                next_use: never_as_none(next),
            },
        });
        frame
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        FrameState::Opt {
            next_use: never_as_none(self.frame_next[frame]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::test_util::{at, log};

    #[test]
    fn evicts_furthest_next_use() {
        // Trace positions 0..3 load pages whose next uses are 10, never, 5.
        let next_use = vec![10, usize::MAX, 5];
        let mut p = Opt::new(3, next_use);
        let mut l = log();
        for f in 0..3 {
            p.on_load(f, at(f, 0), &mut l);
        }
        assert_eq!(p.state(2), FrameState::Opt { next_use: Some(5) });
        assert_eq!(p.victim(at(3, 0), None, &mut l), 1);
        assert_eq!(
            l.take(),
            [Note::Evict {
                frame: 1,
                why: Why::FurthestNextUse { next_use: None }
            }]
        );
    }
}
