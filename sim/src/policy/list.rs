//! A doubly linked list of frames, threaded through two index arrays, so a
//! frame can be moved, inserted or removed in constant time. LRU and
//! InnoDB-style keep their recency order in one.

use crate::FrameIdx;

const NIL: usize = usize::MAX;

pub struct FrameList {
    prev: Vec<usize>,
    next: Vec<usize>,
    linked: Vec<bool>,
    head: usize,
    tail: usize,
    len: usize,
}

impl FrameList {
    pub fn new(frames: usize) -> Self {
        Self {
            prev: vec![NIL; frames],
            next: vec![NIL; frames],
            linked: vec![false; frames],
            head: NIL,
            tail: NIL,
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn contains(&self, frame: FrameIdx) -> bool {
        self.linked[frame]
    }

    pub fn front(&self) -> Option<FrameIdx> {
        some(self.head)
    }

    pub fn back(&self) -> Option<FrameIdx> {
        some(self.tail)
    }

    /// The frame after `frame`, towards the back.
    pub fn next(&self, frame: FrameIdx) -> Option<FrameIdx> {
        some(self.next[frame])
    }

    /// The frame before `frame`, towards the front.
    pub fn prev(&self, frame: FrameIdx) -> Option<FrameIdx> {
        some(self.prev[frame])
    }

    /// Frames between `frame` and the back, not counting `frame`. Walks the
    /// list, so only for the viewer's ranks.
    pub fn behind(&self, frame: FrameIdx) -> usize {
        let mut n = 0;
        let mut at = self.next[frame];
        while at != NIL {
            n += 1;
            at = self.next[at];
        }
        n
    }

    pub fn push_front(&mut self, frame: FrameIdx) {
        match self.front() {
            Some(first) => self.insert_before(frame, first),
            None => self.link(frame, NIL, NIL),
        }
    }

    pub fn push_back(&mut self, frame: FrameIdx) {
        self.link(frame, self.tail, NIL);
    }

    /// Insert `frame` just in front of `at`.
    pub fn insert_before(&mut self, frame: FrameIdx, at: FrameIdx) {
        debug_assert!(self.linked[at], "frame {at} is not in the list");
        self.link(frame, self.prev[at], at);
    }

    pub fn remove(&mut self, frame: FrameIdx) {
        debug_assert!(self.linked[frame], "frame {frame} is not in the list");
        let (p, n) = (self.prev[frame], self.next[frame]);
        if p == NIL {
            self.head = n;
        } else {
            self.next[p] = n;
        }
        if n == NIL {
            self.tail = p;
        } else {
            self.prev[n] = p;
        }
        self.prev[frame] = NIL;
        self.next[frame] = NIL;
        self.linked[frame] = false;
        self.len -= 1;
    }

    /// Move `frame` to the front, linking it first if it is not in the list.
    pub fn move_to_front(&mut self, frame: FrameIdx) {
        if self.linked[frame] {
            self.remove(frame);
        }
        self.push_front(frame);
    }

    /// The back frame, or the one in front of it when the back is `pinned`.
    pub fn back_except(&self, pinned: Option<FrameIdx>) -> Option<FrameIdx> {
        let back = self.back()?;
        if Some(back) == pinned {
            self.prev(back)
        } else {
            Some(back)
        }
    }

    /// Frames from front to back.
    pub fn iter(&self) -> impl Iterator<Item = FrameIdx> + '_ {
        std::iter::successors(self.front(), |&f| self.next(f))
    }

    fn link(&mut self, frame: FrameIdx, p: usize, n: usize) {
        debug_assert!(!self.linked[frame], "frame {frame} is already in the list");
        self.prev[frame] = p;
        self.next[frame] = n;
        if p == NIL {
            self.head = frame;
        } else {
            self.next[p] = frame;
        }
        if n == NIL {
            self.tail = frame;
        } else {
            self.prev[n] = frame;
        }
        self.linked[frame] = true;
        self.len += 1;
    }
}

fn some(i: usize) -> Option<FrameIdx> {
    (i != NIL).then_some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(l: &FrameList) -> Vec<FrameIdx> {
        l.iter().collect()
    }

    #[test]
    fn keeps_order_through_every_operation() {
        let mut l = FrameList::new(6);
        for f in [2, 0, 4] {
            l.push_back(f);
        }
        l.push_front(5);
        l.insert_before(1, 4);
        assert_eq!(order(&l), [5, 2, 0, 1, 4]);
        l.move_to_front(1);
        l.remove(4);
        l.move_to_front(3);
        assert_eq!(order(&l), [3, 1, 5, 2, 0]);
        assert_eq!((l.len(), l.front(), l.back()), (5, Some(3), Some(0)));
        assert_eq!((l.prev(5), l.next(5), l.behind(5)), (Some(1), Some(2), 2));
        assert_eq!(l.back_except(Some(0)), Some(2));
        l.remove(3);
        l.remove(0);
        assert_eq!(order(&l), [1, 5, 2]);
        assert!(!l.contains(0) && l.contains(5));
    }

    #[test]
    fn an_emptied_list_can_be_refilled() {
        let mut l = FrameList::new(2);
        l.push_front(1);
        l.remove(1);
        assert_eq!(
            (l.len(), l.front(), l.back(), l.back_except(None)),
            (0, None, None, None)
        );
        l.push_front(0);
        l.push_front(1);
        assert_eq!(order(&l), [1, 0]);
    }
}
