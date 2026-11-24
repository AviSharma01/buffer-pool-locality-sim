//! LRU: evict the page whose last use is oldest.

use super::list::FrameList;
use super::{Ctx, FrameState, Log, Note, Policy, Why};
use crate::FrameIdx;

pub struct Lru {
    /// Trace position of each frame's last use, for the explanation.
    last_used: Vec<Option<usize>>,
    /// Most recently used at the front. Pages loaded in one request are
    /// linked in turn, and the page asked for moves to the front again when
    /// its request is done, so pages read ahead with it count as older: they
    /// have not been used yet.
    recency: FrameList,
}

impl Lru {
    pub fn new(frames: usize) -> Self {
        Self {
            last_used: vec![None; frames],
            recency: FrameList::new(frames),
        }
    }

    fn used(&mut self, frame: FrameIdx, ctx: Ctx) {
        self.last_used[frame] = Some(ctx.pos);
        self.recency.move_to_front(frame);
    }
}

impl Policy for Lru {
    fn on_hit(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.used(frame, ctx);
    }

    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.used(frame, ctx);
    }

    fn on_request_done(&mut self, requested: FrameIdx) {
        self.recency.move_to_front(requested);
    }

    fn victim(&mut self, _ctx: Ctx, pinned: Option<FrameIdx>, log: &mut Log) -> FrameIdx {
        let frame = self
            .recency
            .back_except(pinned)
            .expect("pool has an unpinned frame");
        let last_used = self.last_used[frame].expect("resident");
        log.push(Note::Evict {
            frame,
            why: Why::LeastRecent { last_used },
        });
        frame
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        FrameState::Lru {
            rank: self.recency.behind(frame),
        }
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
