// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! An upload sent in parts, so a dropped connection costs one part rather
//! than the whole file (docs/plans/acquisition.md A3).
//!
//! The server chooses the part size; every part but the last is exactly that
//! long, and the last holds what is left. A part's place in the file follows
//! from its number alone, so parts may arrive in any order, and a part sent
//! again replaces itself.

/// The part size the server chooses: big enough that a large image is a
/// few hundred requests, small enough that losing one costs little.
pub const PART_BYTES: u64 = 64 * 1024 * 1024;

/// How an upload of `total` bytes divides into parts of `part_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartPlan {
    total: u64,
    part_size: u64,
}

impl PartPlan {
    /// A plan for `total` bytes in parts of `part_size`, or `None` for a part
    /// size of zero.
    #[must_use]
    pub const fn new(total: u64, part_size: u64) -> Option<Self> {
        if part_size == 0 {
            None
        } else {
            Some(Self { total, part_size })
        }
    }

    /// The whole file's length.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Every part's length but the last's.
    #[must_use]
    pub const fn part_size(&self) -> u64 {
        self.part_size
    }

    /// How many parts there are. An empty file has none.
    #[must_use]
    pub const fn part_count(&self) -> u64 {
        self.total.div_ceil(self.part_size)
    }

    /// Where part `number` (from 0) starts and how long it is, or `None` for
    /// a number past the last part.
    #[must_use]
    pub fn part(&self, number: u64) -> Option<(u64, u64)> {
        if number >= self.part_count() {
            return None;
        }
        let offset = number.checked_mul(self.part_size)?;
        Some((offset, self.part_size.min(self.total - offset)))
    }

    /// The part numbers not in `received`, in order.
    #[must_use]
    pub fn missing(&self, received: &[u64]) -> Vec<u64> {
        (0..self.part_count())
            .filter(|number| !received.contains(number))
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_file_divides_into_whole_parts_and_a_remainder() {
        let plan = PartPlan::new(250, 100).unwrap();
        assert_eq!(plan.part_count(), 3);
        assert_eq!(plan.part(0), Some((0, 100)));
        assert_eq!(plan.part(1), Some((100, 100)));
        assert_eq!(plan.part(2), Some((200, 50)));
        assert_eq!(plan.part(3), None);
        assert_eq!(plan.missing(&[1]), [0, 2]);
    }

    #[test]
    fn an_exact_multiple_has_no_short_last_part() {
        let plan = PartPlan::new(300, 100).unwrap();
        assert_eq!(plan.part_count(), 3);
        assert_eq!(plan.part(2), Some((200, 100)));
    }

    #[test]
    fn small_empty_and_huge_files() {
        let small = PartPlan::new(10, PART_BYTES).unwrap();
        assert_eq!(small.part_count(), 1);
        assert_eq!(small.part(0), Some((0, 10)));
        let empty = PartPlan::new(0, PART_BYTES).unwrap();
        assert_eq!(empty.part_count(), 0);
        assert_eq!(empty.part(0), None);
        assert!(empty.missing(&[]).is_empty());
        let huge = PartPlan::new(u64::MAX, PART_BYTES).unwrap();
        let last = huge.part_count() - 1;
        let (offset, length) = huge.part(last).unwrap();
        assert_eq!(offset + length, u64::MAX);
        assert!(PartPlan::new(10, 0).is_none());
    }
}
