//! LRU: evict the page whose last reference is oldest.
//! O(frames) per eviction, which is fine at simulator scale.

use super::{Policy, argmin};
use crate::{FrameIdx, PageId};

pub struct Lru {
    last_used: Vec<usize>,
}

impl Lru {
    pub fn new(frames: usize) -> Self {
        Self {
            last_used: vec![0; frames],
        }
    }
}

impl Policy for Lru {
    fn on_hit(&mut self, frame: FrameIdx, _page: PageId, pos: usize) {
        self.last_used[frame] = pos;
    }

    fn on_load(&mut self, frame: FrameIdx, _page: PageId, pos: usize) {
        self.last_used[frame] = pos;
    }

    fn victim(&mut self) -> FrameIdx {
        argmin(self.last_used.iter().copied().enumerate()).expect("pool has frames")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let mut p = Lru::new(3);
        for f in 0..3 {
            p.on_load(f, f as PageId, f);
        }
        p.on_hit(0, 0, 3);
        assert_eq!(p.victim(), 1);
    }
}
