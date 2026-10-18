//! Deterministic test fixtures.
//!
//! Every fixture is a pure function of `(seed, length)`; two call sites on
//! the same arguments always produce identical bytes, and any byte can be
//! recomputed independently via `byte_at`. This keeps fixture tests honest
//! without a hashing dependency: tests compare content exactly and spot-check
//! positions against the same closed-form rule the generator uses.
//!
//! Test-support note: this module is compiled into every integration test
//! binary that declares `mod common;`, and each binary only exercises the
//! helpers it needs, so file-level `dead_code` is expected and allowed.

#![allow(dead_code)]

use std::sync::Arc;

/// Pattern byte at fixture position `index` for seed `seed`.
///
/// Position-sensitive and seed-sensitive (not a single repeated byte), and
/// never degenerate (not all-zero). Splitmix-style mixing keeps every output
/// bit dependent on both inputs, so distinct seeds effectively never share a
/// byte at the same position. Both generator and tests use this function, so
/// the "expected" values are provably resettable, not magic constants.
///
/// # Panics
/// Never; wraps on overflow by design.
#[must_use]
pub fn pattern_byte(seed: u64, index: u64) -> u8 {
    let mut z = index.wrapping_mul(0x9E_37_79_B9_7F_4A_7C_15);
    z ^= seed;
    z = z.wrapping_mul(0xBF_58_47_6D_1C_E4_E5_B9);
    z = z.rotate_left(31);
    z = z.wrapping_mul(0x94_D0_49_BB_13_31_11_EB);
    (z >> 24) as u8
}

/// A deterministic fixture of exactly `len` bytes derived from `seed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fixture {
    pub seed: u64,
    pub len: u64,
}

impl Fixture {
    #[must_use]
    pub fn new(seed: u64, len: u64) -> Self {
        Self { seed, len }
    }

    /// Recomputes the byte at `index` without materializing the fixture.
    ///
    /// # Panics
    /// Panics if `index >= len`.
    #[must_use]
    pub fn byte_at(&self, index: u64) -> u8 {
        assert!(index < self.len, "fixture index out of range");
        pattern_byte(self.seed, index)
    }

    /// Materializes the full fixture.
    #[must_use]
    pub fn bytes(&self) -> Arc<Vec<u8>> {
        Arc::new((0..self.len).map(|i| pattern_byte(self.seed, i)).collect())
    }

    /// Materializes the inclusive slice `[start, end]`.
    ///
    /// `end` is clamped to `len - 1`, so callers may pass overlong ends to
    /// mean "to the end" without a separate guard in the mock server.
    ///
    /// # Panics
    /// Panics if `start > self.len - 1` or the (clamped) range is empty.
    #[must_use]
    pub fn slice(&self, start: u64, end: u64) -> Arc<Vec<u8>> {
        let end = end.min(self.len.saturating_sub(1));
        assert!(start <= end, "empty or reversed fixture slice requested");
        Arc::new((start..=end).map(|i| pattern_byte(self.seed, i)).collect())
    }
}

impl Default for Fixture {
    /// The default mock payload: 4_096 bytes from seed 1.
    fn default() -> Self {
        Self::new(1, 4_096)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_and_length_reproduce_identical_content() {
        let first = Fixture::new(7, 512).bytes();
        let second = Fixture::new(7, 512).bytes();
        assert_eq!(first, second, "fixtures must be deterministic");
        assert_eq!(first.len(), 512);
    }

    #[test]
    fn different_seeds_produce_different_content() {
        let a = Fixture::new(1, 128).bytes();
        let b = Fixture::new(2, 128).bytes();
        assert_ne!(a, b, "distinct seeds must not collide on the same length");
    }

    #[test]
    fn byte_at_matches_the_materialized_fixture() {
        let fixture = Fixture::new(42, 1_000);
        let bytes = fixture.bytes();
        for index in [0u64, 1, 2, 511, 512, 998, 999] {
            assert_eq!(fixture.byte_at(index), bytes[index as usize]);
        }
    }

    #[test]
    fn slice_is_inclusive_and_clamped() {
        let fixture = Fixture::new(5, 64);
        let exact = fixture.slice(10, 19);
        let clamped = fixture.slice(10, 999);
        assert_eq!(exact.len(), 10);
        assert_eq!(clamped.len(), 64 - 10);
        for (offset, expected) in (10..=19).enumerate() {
            assert_eq!(exact[offset], fixture.byte_at(expected));
        }
    }

    #[test]
    fn content_is_not_degenerate() {
        let bytes = Fixture::new(3, 256).bytes();
        let distinct: std::collections::HashSet<&u8> = bytes.iter().collect();
        assert!(
            distinct.len() >= 16,
            "256 patterned bytes must contain real variety, got {} distinct",
            distinct.len()
        );
    }

    #[test]
    fn zero_length_and_tiny_fixtures_are_supported() {
        assert!(Fixture::new(9, 0).bytes().is_empty());
        assert_eq!(Fixture::new(9, 1).bytes().len(), 1);
    }
}
