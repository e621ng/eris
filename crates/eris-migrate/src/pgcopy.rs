//! Encoder for Postgres `COPY ... FROM STDIN (FORMAT binary)` frames for the
//! `images` table columns (post_id int4, avglf1..3 float8, sig bytea).
//! Everything in the binary COPY format is big-endian; note the contrast with
//! the little-endian i16s *inside* the sig blob, which is opaque bytea here.

use crate::sqlite::SigRow;

const HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0";

pub struct CopyEncoder {
  buf: Vec<u8>,
}

impl CopyEncoder {
  pub fn new() -> Self {
    let mut buf = Vec::with_capacity(1 << 20);
    buf.extend_from_slice(HEADER);
    buf.extend_from_slice(&0u32.to_be_bytes()); // flags
    buf.extend_from_slice(&0u32.to_be_bytes()); // header extension length
    CopyEncoder { buf }
  }

  pub fn push_row(&mut self, row: &SigRow) {
    self.buf.extend_from_slice(&5i16.to_be_bytes()); // field count
    self.buf.extend_from_slice(&4i32.to_be_bytes());
    self.buf.extend_from_slice(&row.post_id.to_be_bytes());
    for value in row.avglf {
      self.buf.extend_from_slice(&8i32.to_be_bytes());
      self.buf.extend_from_slice(&value.to_bits().to_be_bytes());
    }
    self
      .buf
      .extend_from_slice(&(row.sig.len() as i32).to_be_bytes());
    self.buf.extend_from_slice(&row.sig);
  }

  /// Drain the accumulated bytes for streaming; the encoder stays usable.
  pub fn take(&mut self) -> Vec<u8> {
    std::mem::take(&mut self.buf)
  }

  pub fn len(&self) -> usize {
    self.buf.len()
  }

  /// The end-of-data trailer, sent once after all rows.
  pub fn trailer() -> [u8; 2] {
    (-1i16).to_be_bytes()
  }
}

impl Default for CopyEncoder {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn exact_byte_layout() {
    let mut enc = CopyEncoder::new();
    let row = SigRow {
      post_id: 0x01020304,
      avglf: [1.0, -0.0, 0.5],
      sig: vec![0xAA, 0xBB],
    };
    enc.push_row(&row);
    let bytes = enc.take();

    let mut expected = Vec::new();
    expected.extend_from_slice(b"PGCOPY\n\xff\r\n\0");
    expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // flags + ext
    expected.extend_from_slice(&[0, 5]); // field count
    expected.extend_from_slice(&[0, 0, 0, 4, 1, 2, 3, 4]); // int4
    expected.extend_from_slice(&[0, 0, 0, 8, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0]); // 1.0
    expected.extend_from_slice(&[0, 0, 0, 8, 0x80, 0, 0, 0, 0, 0, 0, 0]); // -0.0
    expected.extend_from_slice(&[0, 0, 0, 8, 0x3f, 0xe0, 0, 0, 0, 0, 0, 0]); // 0.5
    expected.extend_from_slice(&[0, 0, 0, 2, 0xAA, 0xBB]); // bytea
    assert_eq!(bytes, expected);
    assert_eq!(CopyEncoder::trailer(), [0xff, 0xff]);
  }
}
