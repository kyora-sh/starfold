//! The web client's `codecs.test.ts` AV1 cases, against the server's port.

use super::super::av1::{codec_string, sequence_header};

#[derive(Default)]
struct BitWriter {
    bits: Vec<u8>,
}

impl BitWriter {
    fn f(mut self, n: u32, v: u64) -> Self {
        self.bits.extend((0..n).rev().map(|i| (v >> i) as u8 & 1));
        self
    }

    fn bytes(self) -> Vec<u8> {
        self.bits
            .chunks(8)
            .map(|c| c.iter().enumerate().fold(0, |b, (i, &x)| b | x << (7 - i)))
            .collect()
    }
}

/// A temporal delimiter, then the sequence header OBU with a size field.
fn temporal_unit(seq: &[u8]) -> Vec<u8> {
    let mut d = vec![0x12, 0x00, 0x0a, seq.len() as u8];
    d.extend_from_slice(seq);
    d
}

#[test]
fn the_8_bit_design_example() {
    let mut w = BitWriter::default();
    for (n, v) in [
        (3, 0),
        (1, 0),
        (1, 0),
        (1, 0),
        (1, 0),
        (5, 0),
        (12, 0),
        (5, 13),
        (1, 0),
        (4, 11),
        (4, 11),
        (12, 2559),
        (12, 1599),
        (1, 0),
        (3, 0b011),
        (4, 0),
        (1, 0),
        (1, 1),
        (1, 1),
        (3, 0b011),
        (1, 0),
        (1, 0),
        (1, 1),
        (8, 1),
        (8, 13),
        (8, 6),
        (1, 0),
        (2, 0),
        (1, 0),
        (1, 0),
    ] {
        w = w.f(n, v);
    }
    assert_eq!(
        codec_string(&temporal_unit(&w.bytes())).as_deref(),
        Some("av01.0.13M.08.0.110.01.13.06.0")
    );
}

#[test]
fn timing_info_two_operating_points_high_tier_10_bit_full_range() {
    let mut w = BitWriter::default();
    for (n, v) in [
        (3, 0),
        (1, 0),
        (1, 0),
        (1, 1),
        (32, 1001),
        (32, 60000),
        (1, 1),
        (1, 1),
        (1, 1),
        (5, 9),
        (32, 1),
        (5, 4),
        (5, 4),
        (1, 1),
        (5, 1),
        (12, 0x103),
        (5, 16),
        (1, 1),
        (1, 1),
        (10, 300),
        (10, 200),
        (1, 0),
        (1, 1),
        (4, 3),
        (12, 0x101),
        (5, 5),
        (1, 0),
        (1, 0),
        (4, 15),
        (4, 15),
        (16, 3839),
        (16, 2159),
        (1, 1),
        (4, 2),
        (3, 1),
        (3, 0b111),
        (4, 0),
        (1, 1),
        (2, 0b11),
        (1, 0),
        (1, 1),
        (1, 0),
        (1, 1),
        (3, 6),
        (3, 0b011),
        (1, 1),
        (1, 0),
        (1, 1),
        (8, 9),
        (8, 16),
        (8, 9),
        (1, 1),
        (2, 2),
        (1, 0),
        (1, 0),
    ] {
        w = w.f(n, v);
    }
    assert_eq!(
        codec_string(&temporal_unit(&w.bytes())).as_deref(),
        Some("av01.0.16H.10.0.112.09.16.09.1")
    );
}

#[test]
fn the_committed_fixture() {
    let ivf = include_bytes!("../../../../../bench/tests/fixtures/av1-640x360.ivf");
    let size = u32::from_le_bytes(ivf[32..36].try_into().unwrap()) as usize;
    let first = &ivf[44..44 + size];
    assert!(sequence_header(first).is_some());
    assert_eq!(codec_string(first).as_deref(), Some("av01.0.01M.08"));
}

#[test]
fn none_without_a_sequence_header_or_from_a_truncated_one() {
    assert_eq!(codec_string(&[0x12, 0x00, 0x32, 0x01, 0x00]), None);
    assert_eq!(codec_string(&[0x0a, 0x02, 0x00, 0x00]), None);
    assert_eq!(codec_string(&[0x0a, 0x80]), None);
    assert_eq!(
        codec_string(&[0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
        None
    );
}
