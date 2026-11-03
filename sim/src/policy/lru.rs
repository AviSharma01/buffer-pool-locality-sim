//! LRU: evict the page whose last reference is oldest.
//! O(frames) per eviction, which is fine at simulator scale.

use super::{Ctx, FrameState, Log, Note, Policy, Why, argmin};
use crate::FrameIdx;

pub struct Lru {
    /// Trace position of each frame's last reference; None while empty.
    last_used: Vec<Option<usize>>,
}

impl Lru {
    pub fn new(frames: usize) -> Self {
        Self {
            last_used: vec![None; frames],
        }
    }

    fn resident(&self) -> impl Iterator<Item = (FrameIdx, usize)> + '_ {
        self.last_used
            .iter()
            .enumerate()
            .filter_map(|(f, t)| t.map(|t| (f, t)))
    }
}

impl Policy for Lru {
    fn on_hit(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.last_used[frame] = Some(ctx.pos);
    }

    fn on_load(&mut self, frame: FrameIdx, ctx: Ctx, _log: &mut Log) {
        self.last_used[frame] = Some(ctx.pos);
    }

    fn victim(&mut self, _ctx: Ctx, log: &mut Log) -> FrameIdx {
        let frame = argmin(self.resident()).expect("pool has frames");
        let last_used = self.last_used[frame].expect("resident");
        log.push(Note::Evict {
            frame,
            why: Why::LeastRecent { last_used },
        });
        frame
    }

    fn state(&self, frame: FrameIdx) -> FrameState {
        let mine = (
            self.last_used[frame].expect("state of a resident frame"),
            frame,
        );
        let rank = self.resident().filter(|&(f, t)| (t, f) < mine).count();
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
        assert_eq!(p.victim(at(4, 0), &mut l), 1);
        assert_eq!(
            l.take(),
            [Note::Evict {
                frame: 1,
                why: Why::LeastRecent { last_used: 1 }
            }]
        );
    }
}
