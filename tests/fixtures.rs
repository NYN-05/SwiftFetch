//! Phase 2 integration tests for deterministic fixture generation
//! (DOCS/phases.md §5: "Fixture tests verifying generated files and expected
//! hashes").
//!
//! Golden values (first bytes and FNV-1a checksums) were recorded from the
//! closed-form generator on Windows. They exist so an accidental change to
//! `pattern_byte` fails loudly; the recorded contract is intentionally
//! strict. The checksum is five inline lines, not a hashing dependency.

mod common;

use common::fixture::{pattern_byte, Fixture};

/// FNV-1a 64-bit over the whole input, computed inline so fixtures have a
/// stable content fingerprint without adding a crypto crate in Phase 2.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcb_f2_9c_e4_84_22_23_25;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[test]
fn default_fixture_is_the_4096_byte_seed_1_pattern() {
    let default_fixture = Fixture::default();
    assert_eq!(default_fixture, Fixture::new(1, 4_096));
    assert_eq!(default_fixture.seed, 1);
    assert_eq!(default_fixture.len, 4_096);
    assert_eq!(default_fixture.bytes().len(), 4_096);
}

#[test]
fn golden_first_32_bytes_are_stable() {
    let bytes = Fixture::default().bytes();
    // Recorded from the closed-form generator; changing `pattern_byte` must
    // fail this test loudly.
    const GOLDEN_FIRST_32: [u8; 32] = [
        0x19, 0x15, 0x9D, 0x2D, 0x34, 0xB1, 0x39, 0x35, 0xD0, 0x4D, 0x54, 0xD1, 0x6C, 0x68, 0xF0,
        0x6C, 0x74, 0x04, 0x8C, 0x88, 0x10, 0x8C, 0xA7, 0x24, 0xAC, 0xA8, 0x43, 0xC0, 0xC7, 0x44,
        0xCC, 0xDB,
    ];
    for (index, expected) in GOLDEN_FIRST_32.iter().enumerate() {
        assert_eq!(
            bytes[index], *expected,
            "fixture byte {index} drifted from the golden record"
        );
    }
}

#[test]
fn golden_checksum_of_the_full_default_fixture_is_stable() {
    let bytes = Fixture::default().bytes();
    const GOLDEN_DEFAULT_FNV1A: u64 = 0xf4af_2eff_d981_021c;
    assert_eq!(fnv1a64(&bytes), GOLDEN_DEFAULT_FNV1A);
}

#[test]
fn golden_checksum_stays_stable_for_a_second_fixture_shape() {
    let bytes = Fixture::new(7, 1_000).bytes();
    const GOLDEN_SEED7_LEN1000_FNV1A: u64 = 0x8df6_ef2d_7746_055c;
    assert_eq!(fnv1a64(&bytes), GOLDEN_SEED7_LEN1000_FNV1A);
}

#[test]
fn every_byte_at_matches_the_materialized_pattern() {
    let fixture = Fixture::new(11, 600);
    let bytes = fixture.bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        assert_eq!(byte, pattern_byte(11, index as u64));
        assert_eq!(byte, fixture.byte_at(index as u64));
    }
}

#[test]
fn slices_are_exact_windows_of_the_full_fixture() {
    let fixture = Fixture::new(3, 128);
    let full = fixture.bytes();
    for (start, end) in [(0u64, 0u64), (0, 127), (5, 9), (63, 64), (127, 127)] {
        assert_eq!(
            fixture.slice(start, end).as_ref(),
            &full[start as usize..=end as usize],
            "slice [{start}, {end}] must be the exact window"
        );
    }
}

#[test]
fn distinct_seeds_produce_distinct_content_at_the_same_length() {
    let first = Fixture::new(1, 512).bytes();
    let second = Fixture::new(2, 512).bytes();
    assert_ne!(first, second, "seeds 1 and 2 must not share content");
}

#[test]
fn same_index_across_seeds_yields_mostly_distinct_bytes() {
    let seeds = [1u64, 2, 3, 5, 8];
    for index in [0u64, 7, 63, 511] {
        let values: std::collections::HashSet<u8> = seeds
            .iter()
            .map(|seed| pattern_byte(*seed, index))
            .collect();
        assert!(
            values.len() >= 4,
            "distinct seeds should produce mostly distinct bytes at index {index}"
        );
    }
}

#[test]
#[should_panic(expected = "fixture index out of range")]
fn byte_at_out_of_range_panics() {
    let fixture = Fixture::new(1, 10);
    let _ = fixture.byte_at(10);
}
