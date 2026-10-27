//! FIFO: evict the page that was loaded longest ago, ignoring hits.
//! Suffers from Belady's anomaly (more frames can mean more misses).

use std::collections::VecDeque;

use super::Policy;
use crate::{FrameIdx, PageId};

pub struct Fifo {
    queue: VecDeque<FrameIdx>,
}

impl Fifo {
    pub fn new(frames: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(frames),
        }
    }
}

impl Policy for Fifo {
    fn on_hit(&mut self, _frame: FrameIdx, _page: PageId, _pos: usize) {}

    fn on_load(&mut self, frame: FrameIdx, _page: PageId, _pos: usize) {
        self.queue.push_back(frame);
    }

    fn victim(&mut self) -> FrameIdx {
        self.queue
            .pop_front()
            .expect("victim called with no resident pages")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_in_load_order_regardless_of_hits() {
        let mut p = Fifo::new(3);
        for f in 0..3 {
            p.on_load(f, f as PageId, f);
        }
        p.on_hit(0, 0, 3);
        assert_eq!(p.victim(), 0);
        p.on_load(0, 9, 4);
        assert_eq!(p.victim(), 1);
    }
}
