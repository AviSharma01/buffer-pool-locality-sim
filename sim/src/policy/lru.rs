//! LRU: evict the page whose last use is oldest.
//! O(frames) per eviction, which is fine at simulator scale.

use super::{Ctx, FrameState, Log, Note, Policy, Why};
use crate::FrameIdx;

pub struct Lru {
    /// Trace position of each frame's last use, for the explanation; None
    /// while empty.
    last_used: Vec<Option<usize>>,
    /// Recency order: a counter that only goes up, stamped at every use.
    /// Pages loaded in one request get distinct stamps, and the page asked
    /// for is stamped again when its request is done, so pages read ahead
    /// with it count as older: they have not been used yet.
    stamp: Vec<u64>,
    clock: u64,
}

impl Lru {
    pub fn new(frames: usize) -> Self {
        Self {
            last_used: vec![None; frames],
            stamp: vec![0; frames],
            clock: 0,
        }
    }

    fn touch(&mut self, frame: FrameIdx) {
        self.clock += 1;
        self.stamp[frame] = self.clock;
    }

    /// Resident frames with their recency stamp.
    fn resident(&self) -> impl Iterator<Item = (FrameIdx, u64)> + '_ {
        self.last_used
            .iter()
            .enumerate()
            .filter(|(_, t)| t.is_some())
            .map(|(f, _)| (f, self.stamp[f]))
    }
}

impl Policy for Lru {
    fn on_hit(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.last_used[frame] = Some(ctx.pos);
        self.touch(frame);
    }

    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.last_used[frame] = Some(ctx.pos);
        self.touch(frame);
    }

    fn on_request_done(&mut self, requested: FrameIdx) {
        self.touch(requested);
    }

    fn victim(&mut self, _ctx: Ctx, pinned: Option<FrameIdx>, log: &mut Log) -> FrameIdx {
        let (frame, _) = self
            .resident()
            .filter(|&(f, _)| Some(f) != pinned)
            .min_by_key(|&(_, stamp)| stamp)
            .expect("pool has an unpinned frame");
        let last_used = self.last_used[frame].expect("resident");
        log.push(Note::Evict {
            frame,
            why: Why::LeastRecent { last_used },
        });
        frame
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        let mine = self.stamp[frame];
        let rank = self.resident().filter(|&(_, t)| t < mine).count();
        FrameState::Lru { rank }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::test_util::{at, log};

    #[test]
    fn evicts_least_recently_used() {
        let mut p = Lru::new(3);
        let mut l = log();
        for f in 0..3 {
            p.on_load(f, at(f, 0), &mut l);
        }
        p.on_hit(0, at(3, 0), &mut l);
        assert_eq!(p.state(0), FrameState::Lru { rank: 2 });
        assert_eq!(p.state(1), FrameState::Lru { rank: 0 });
        assert_eq!(p.victim(at(4, 0), None, &mut l), 1);
        assert_eq!(
            l.take(),
            [Note::Evict {
                frame: 1,
                why: Why::LeastRecent { last_used: 1 }
            }]
        );
    }
}
