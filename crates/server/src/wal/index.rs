//! Where the log holds each page.

use std::collections::HashMap;

/// The frames that hold each page, oldest first.
///
/// A list and not one offset, because a reader asks for the newest frame at or
/// before a mark of its own, and the frame it wants is not always the last.
#[derive(Debug, Default)]
pub struct FrameIndex {
    frames: HashMap<(u32, u32), Vec<u64>>,
}

impl FrameIndex {
    /// Notes a frame for a page. Frames arrive in the order they were written.
    pub fn insert(&mut self, table: u32, page_no: u32, at: u64) {
        self.frames.entry((table, page_no)).or_default().push(at);
    }

    /// The newest frame for a page that starts before `upto`, or none when the
    /// log holds no such frame and the table file answers instead.
    pub fn newest(&self, table: u32, page_no: u32, upto: u64) -> Option<u64> {
        self.frames
            .get(&(table, page_no))
            .and_then(|frames| frames.iter().rev().find(|at| **at < upto))
            .copied()
    }

    /// Every page the log holds, with its newest frame.
    pub fn pages(&self) -> Vec<(u32, u32, u64)> {
        self.frames
            .iter()
            .filter_map(|((table, page_no), frames)| {
                frames.last().map(|at| (*table, *page_no, *at))
            })
            .collect()
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_the_log_holds_not_is_not_in_the_index() {
        let index = FrameIndex::default();
        assert_eq!(index.newest(1, 2, u64::MAX), None);
        assert!(index.pages().is_empty());
    }

    #[test]
    fn the_newest_frame_of_a_page_is_the_one_answered() {
        let mut index = FrameIndex::default();
        index.insert(1, 2, 100);
        index.insert(1, 2, 300);
        index.insert(1, 3, 200);
        assert_eq!(index.newest(1, 2, u64::MAX), Some(300));
        assert_eq!(index.newest(1, 3, u64::MAX), Some(200));
    }

    #[test]
    fn a_mark_hides_a_frame_written_after_it() {
        let mut index = FrameIndex::default();
        index.insert(1, 2, 100);
        index.insert(1, 2, 300);
        assert_eq!(index.newest(1, 2, 301), Some(300));
        assert_eq!(
            index.newest(1, 2, 300),
            Some(100),
            "the later frame is hidden"
        );
        assert_eq!(index.newest(1, 2, 100), None, "both are hidden");
    }

    #[test]
    fn one_table_does_not_answer_for_another() {
        let mut index = FrameIndex::default();
        index.insert(1, 2, 100);
        assert_eq!(index.newest(2, 2, u64::MAX), None);
    }

    #[test]
    fn every_page_comes_back_with_its_newest_frame() {
        let mut index = FrameIndex::default();
        index.insert(1, 2, 100);
        index.insert(1, 2, 300);
        index.insert(7, 9, 200);
        let mut pages = index.pages();
        pages.sort_unstable();
        assert_eq!(pages, vec![(1, 2, 300), (7, 9, 200)]);

        index.clear();
        assert!(index.pages().is_empty());
        assert_eq!(index.newest(1, 2, u64::MAX), None);
    }
}
