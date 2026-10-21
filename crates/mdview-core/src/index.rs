// SPDX-License-Identifier: Apache-2.0
//! Scroll position index: measured-vs-estimated block heights.
//!
//! The whole document never needs to be laid out. Every block has an
//! estimated height from the start (cheap heuristics from character count
//! and line count); as blocks scroll near the viewport they are really laid
//! out and their measured height replaces the estimate. A prefix-sum array
//! then answers "which blocks intersect this scroll offset" in O(log n),
//! and the scrollbar reflects `total` - estimates converge to measurements
//! as the user scrolls.

/// Height estimator supplied by the caller (knows font metrics + width).
pub trait HeightEstimator {
    /// Rough height in physical pixels for block `idx`.
    fn estimate(&mut self, idx: usize) -> f32;
    /// Vertical margins around a block (same for all blocks of a kind run).
    fn block_margin(&self, idx: usize) -> f32;
}

pub struct PositionIndex {
    heights: Vec<f32>,
    margins: Vec<f32>,
    measured: Vec<bool>,
    prefix: Vec<f32>,
    total: f32,
    dirty: bool,
}

impl PositionIndex {
    pub fn new(n: usize) -> PositionIndex {
        PositionIndex {
            heights: vec![0.0; n],
            margins: vec![0.0; n],
            measured: vec![false; n],
            prefix: vec![0.0; n + 1],
            total: 0.0,
            dirty: false,
        }
    }

    pub fn len(&self) -> usize {
        self.heights.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    pub fn total(&mut self) -> f32 {
        self.rebuild();
        self.total
    }

    /// Y offset of block `idx`'s top edge, and its height.
    pub fn block_pos(&mut self, idx: usize) -> (f32, f32) {
        self.rebuild();
        (self.prefix[idx], self.heights[idx] + self.margins[idx])
    }

    /// Invalidate all measurements (zoom, resize, DPI change, reload).
    /// Heights revert to estimates on the next `fill_estimates`.
    pub fn reset_measured(&mut self) {
        for m in self.measured.iter_mut() {
            *m = false;
        }
        self.dirty = true;
    }

    /// Ensure every block has at least an estimated height.
    pub fn fill_estimates(&mut self, est: &mut dyn HeightEstimator) {
        for idx in 0..self.heights.len() {
            if !self.measured[idx] {
                self.margins[idx] = est.block_margin(idx);
                self.heights[idx] = est.estimate(idx).max(1.0);
                self.dirty = true;
            }
        }
    }

    /// Record a real, laid-out height for a block.
    pub fn set_measured(&mut self, idx: usize, height: f32) {
        if idx < self.heights.len() {
            self.heights[idx] = height.max(1.0);
            self.measured[idx] = true;
            self.dirty = true;
        }
    }

    /// Record the margin for a block (called with estimates).
    pub fn set_margin(&mut self, idx: usize, margin: f32) {
        if idx < self.margins.len() {
            self.margins[idx] = margin;
            self.dirty = true;
        }
    }

    /// First block index whose top is at or after `y`, clamped to the last
    /// block so the block straddling `y` is included.
    pub fn first_at_or_after(&mut self, y: f32) -> usize {
        self.rebuild();
        self.prefix
            .partition_point(|&p| p <= y)
            .saturating_sub(1)
            .min(self.heights.len().saturating_sub(1))
    }

    /// Y offset for the top of a block (after rebuild).
    pub fn offset_of(&mut self, idx: usize) -> f32 {
        self.rebuild();
        self.prefix[idx.min(self.heights.len())]
    }

    /// Rebuild the prefix sums. O(n) adds over f32 - a 200k-block document
    /// costs well under a millisecond, amortized over measured arrivals.
    pub fn rebuild(&mut self) {
        if !self.dirty {
            return;
        }
        let mut acc = 0.0f32;
        for i in 0..self.heights.len() {
            self.prefix[i] = acc;
            acc += self.heights[i] + self.margins[i];
        }
        self.prefix[self.heights.len()] = acc;
        self.total = acc;
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;
    impl HeightEstimator for Fixed {
        fn estimate(&mut self, _idx: usize) -> f32 {
            10.0
        }
        fn block_margin(&self, _idx: usize) -> f32 {
            2.0
        }
    }

    #[test]
    fn estimates_then_measures() {
        let mut idx = PositionIndex::new(4);
        idx.fill_estimates(&mut Fixed);
        idx.rebuild();
        assert!((idx.total() - 48.0).abs() < 1e-3); // 4 * (10 + 2)

        idx.set_measured(1, 100.0);
        idx.rebuild();
        assert!((idx.total() - 138.0).abs() < 1e-3);

        let (y, h) = idx.block_pos(1);
        assert!((y - 12.0).abs() < 1e-3); // after block 0 (10+2)
        assert!((h - 102.0).abs() < 1e-3);
    }

    #[test]
    fn lookup_window() {
        let mut idx = PositionIndex::new(3);
        idx.fill_estimates(&mut Fixed);
        idx.set_measured(0, 30.0);
        idx.rebuild();
        assert_eq!(idx.first_at_or_after(0.0), 0);
        // Block 0 spans [0, 32) after measuring; y=31 is still inside it.
        assert_eq!(idx.first_at_or_after(31.0), 0);
        assert_eq!(idx.first_at_or_after(32.0), 1);
        assert_eq!(idx.first_at_or_after(1000.0), 2);
    }
}
