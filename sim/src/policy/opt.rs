//! OPT (Belady's MIN): evict the page whose next use is furthest in the
//! future. Needs the whole trace, so it is a yardstick, not a real policy:
//! no policy can have fewer misses on the same trace.

use super::Policy;
use crate::{FrameIdx, PageId};

pub struct Opt {
    /// From [`crate::trace::Trace::next_use`].
    next_use: Vec<usize>,
    /// Next-use position of the page in each frame.
    frame_next: Vec<usize>,
}

impl Opt {
    pub fn new(frames: usize, next_use: Vec<usize>) -> Self {
        Self {
            next_use,
            frame_next: vec![usize::MAX; frames],
        }
    }
}

impl Policy for Opt {
    fn on_hit(&mut self, frame: FrameIdx, _page: PageId, pos: usize) {
        self.frame_next[frame] = self.next_use[pos];
    }

    fn on_load(&mut self, frame: FrameIdx, _page: PageId, pos: usize) {
        self.frame_next[frame] = self.next_use[pos];
    }

    fn victim(&mut self) -> FrameIdx {
        // Furthest next use; ties (e.g. several never-used-again) -> lowest index.
        self.frame_next
            .iter()
            .enumerate()
            .max_by_key(|&(i, &next)| (next, std::cmp::Reverse(i)))
            .map(|(i, _)| i)
            .expect("pool has frames")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_furthest_next_use() {
        // Trace positions 0..3 load pages whose next uses are 10, never, 5.
        let next_use = vec![10, usize::MAX, 5];
        let mut p = Opt::new(3, next_use);
        for f in 0..3 {
            p.on_load(f, f as PageId, f);
        }
        assert_eq!(p.victim(), 1);
    }
}
