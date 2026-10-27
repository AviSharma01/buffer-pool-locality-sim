//! 2Q (Johnson & Shasha, VLDB '94), full version.
//!
//! New pages enter `A1in`, a small FIFO. When evicted from `A1in` their ids
//! are remembered in the ghost list `A1out` (no data). A page that is missed
//! again while still in `A1out` has proven it is re-used and goes to `Am`, an
//! LRU of hot pages. One-off pages (e.g. from a sequential scan) pass through
//! `A1in` and never displace the hot set, which makes 2Q scan resistant.

use std::collections::{HashSet, VecDeque};

use super::{Policy, argmin};
use crate::{FrameIdx, PageId};

pub struct TwoQ {
    /// Max resident pages in A1in (paper's Kin, ~25% of the pool).
    k_in: usize,
    /// Max ghost entries in A1out (paper's Kout, ~50% of the pool).
    k_out: usize,
    a1in: VecDeque<FrameIdx>,
    a1out: VecDeque<PageId>,
    a1out_set: HashSet<PageId>,
    in_am: Vec<bool>,
    last_used: Vec<usize>,
    frame_page: Vec<PageId>,
}

impl TwoQ {
    pub fn new(frames: usize) -> Self {
        Self {
            k_in: (frames / 4).max(1),
            k_out: (frames / 2).max(1),
            a1in: VecDeque::new(),
            a1out: VecDeque::new(),
            a1out_set: HashSet::new(),
            in_am: vec![false; frames],
            last_used: vec![0; frames],
            frame_page: vec![0; frames],
        }
    }

    fn remember_ghost(&mut self, page: PageId) {
        self.a1out.push_back(page);
        self.a1out_set.insert(page);
        if self.a1out.len() > self.k_out {
            if let Some(old) = self.a1out.pop_front() {
                self.a1out_set.remove(&old);
            }
        }
    }
}

impl Policy for TwoQ {
    fn on_hit(&mut self, frame: FrameIdx, _page: PageId, pos: usize) {
        // Hits in A1in are deliberately ignored (correlated references).
        if self.in_am[frame] {
            self.last_used[frame] = pos;
        }
    }

    fn on_load(&mut self, frame: FrameIdx, page: PageId, pos: usize) {
        self.frame_page[frame] = page;
        if self.a1out_set.remove(&page) {
            self.a1out.retain(|&p| p != page);
            self.in_am[frame] = true;
            self.last_used[frame] = pos;
        } else {
            self.in_am[frame] = false;
            self.a1in.push_back(frame);
        }
    }

    fn victim(&mut self) -> FrameIdx {
        let am_empty = !self.in_am.iter().any(|&b| b);
        if self.a1in.len() > self.k_in || am_empty {
            let frame = self
                .a1in
                .pop_front()
                .expect("A1in non-empty when Am is empty");
            self.remember_ghost(self.frame_page[frame]);
            frame
        } else {
            let am = self
                .in_am
                .iter()
                .enumerate()
                .filter(|&(_, &b)| b)
                .map(|(i, _)| (i, self.last_used[i]));
            argmin(am).expect("Am non-empty")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghost_hit_promotes_to_am() {
        let mut p = TwoQ::new(4); // k_in = 1, k_out = 2
        for f in 0..4 {
            p.on_load(f, 100 + f as PageId, f);
        }
        // Am is empty, so A1in's oldest goes first and page 100 becomes a ghost.
        assert_eq!(p.victim(), 0);
        p.on_load(0, 100, 4);
        assert!(p.in_am[0], "page re-missed while a ghost should enter Am");
        // A1in now holds frames 1..3 (> k_in), so it keeps shedding from A1in,
        // protecting the hot page in Am.
        assert_eq!(p.victim(), 1);
        p.on_load(1, 200, 5);
        assert_eq!(p.victim(), 2);
    }
}
