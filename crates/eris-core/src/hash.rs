//! The hex hash codec, wire-compatible with the C++ `HaarSignature::to_string`
//! / `from_hash`: no prefix, 528 hex chars = 3 x 16 hex chars of the raw
//! IEEE-754 bits of each avglf double, then 120 x 4 hex chars of each i16
//! coefficient (two's complement). Lowercase on output; case-insensitive on
//! input (the C++ used sscanf %x). Decoding does NOT re-sort the
//! coefficients: parity requires taking a hash exactly as given.

// Indexed loops keep the wire layout explicit.
#![allow(clippy::needless_range_loop)]

use crate::signature::HaarSignature;
use crate::NUM_COEFS;

/// Length of an encoded hash in characters.
pub const HASH_LEN: usize = 3 * 16 + 3 * NUM_COEFS * 4;

#[derive(Debug, PartialEq, Eq)]
pub enum HashError {
  BadLength(usize),
  BadHex,
}

impl std::fmt::Display for HashError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      HashError::BadLength(n) => write!(f, "hash is {n} chars, expected {HASH_LEN}"),
      HashError::BadHex => write!(f, "hash contains non-hexadecimal characters"),
    }
  }
}

impl std::error::Error for HashError {}

pub fn encode(sig: &HaarSignature) -> String {
  use std::fmt::Write;
  let mut out = String::with_capacity(HASH_LEN);
  for c in 0..3 {
    write!(out, "{:016x}", sig.avglf[c].to_bits()).expect("string write");
  }
  for c in 0..3 {
    for i in 0..NUM_COEFS {
      write!(out, "{:04x}", sig.sig[c][i] as u16).expect("string write");
    }
  }
  debug_assert_eq!(out.len(), HASH_LEN);
  out
}

pub fn decode(s: &str) -> Result<HaarSignature, HashError> {
  if s.len() != HASH_LEN {
    return Err(HashError::BadLength(s.len()));
  }
  if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
    return Err(HashError::BadHex);
  }

  let mut avglf = [0f64; 3];
  for (c, item) in avglf.iter_mut().enumerate() {
    let bits = u64::from_str_radix(&s[c * 16..(c + 1) * 16], 16).map_err(|_| HashError::BadHex)?;
    *item = f64::from_bits(bits);
  }

  let mut sig = [[0i16; NUM_COEFS]; 3];
  let coefs = &s[48..];
  for i in 0..3 * NUM_COEFS {
    let bits =
      u16::from_str_radix(&coefs[i * 4..(i + 1) * 4], 16).map_err(|_| HashError::BadHex)?;
    sig[i / NUM_COEFS][i % NUM_COEFS] = bits as i16;
  }

  Ok(HaarSignature { avglf, sig })
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;

  fn arb_signature() -> impl Strategy<Value = HaarSignature> {
    (
      [any::<u64>(), any::<u64>(), any::<u64>()],
      proptest::collection::vec(any::<i16>(), 120),
    )
      .prop_map(|(bits, coefs)| {
        let mut sig = [[0i16; NUM_COEFS]; 3];
        for (i, &v) in coefs.iter().enumerate() {
          sig[i / NUM_COEFS][i % NUM_COEFS] = v;
        }
        HaarSignature {
          avglf: [
            f64::from_bits(bits[0]),
            f64::from_bits(bits[1]),
            f64::from_bits(bits[2]),
          ],
          sig,
        }
      })
  }

  proptest! {
      #[test]
      fn round_trip(sig in arb_signature()) {
          let encoded = encode(&sig);
          prop_assert_eq!(encoded.len(), HASH_LEN);
          let decoded = decode(&encoded).unwrap();
          // Compare bit patterns (NaN-safe): PartialEq on f64 would fail
          // for NaN avglf values, which the codec must still round-trip.
          for c in 0..3 {
              prop_assert_eq!(sig.avglf[c].to_bits(), decoded.avglf[c].to_bits());
          }
          prop_assert_eq!(sig.sig, decoded.sig);
      }
  }

  #[test]
  fn known_special_values() {
    let sig = HaarSignature {
      avglf: [-0.0, f64::MIN_POSITIVE / 2.0, 1.0], // -0.0 and a subnormal
      sig: [[-1i16; NUM_COEFS]; 3],
    };
    let encoded = encode(&sig);
    assert!(encoded.starts_with("8000000000000000")); // -0.0 bits
    assert!(encoded.ends_with(&"ffff".repeat(120))); // -1 as u16
    let decoded = decode(&encoded).unwrap();
    assert_eq!(decoded.avglf[0].to_bits(), (-0.0f64).to_bits());
    assert_eq!(decoded.sig, sig.sig);
  }

  #[test]
  fn uppercase_accepted() {
    let sig = HaarSignature {
      avglf: [1.5, -2.5, 0.125],
      sig: [[257i16; NUM_COEFS]; 3],
    };
    let upper = encode(&sig).to_uppercase();
    assert_eq!(decode(&upper).unwrap(), sig);
  }

  #[test]
  fn wrong_length_rejected() {
    assert_eq!(decode(""), Err(HashError::BadLength(0)));
    assert_eq!(decode(&"0".repeat(527)), Err(HashError::BadLength(527)));
    assert_eq!(decode(&"0".repeat(529)), Err(HashError::BadLength(529)));
  }

  #[test]
  fn bad_hex_rejected_without_panic() {
    let mut s = "0".repeat(HASH_LEN);
    s.replace_range(10..11, "g");
    assert_eq!(decode(&s), Err(HashError::BadHex));
    // Multi-byte UTF-8 must not slice-panic either.
    let s2 = "\u{00e9}".repeat(HASH_LEN / 2);
    assert!(decode(&s2).is_err());
  }

  #[test]
  fn decode_does_not_resort() {
    // An intentionally unsorted coefficient sequence survives round-trip.
    let mut sig = HaarSignature {
      avglf: [0.1, 0.2, 0.3],
      sig: [[0i16; NUM_COEFS]; 3],
    };
    sig.sig[0][0] = 100;
    sig.sig[0][1] = -100;
    let decoded = decode(&encode(&sig)).unwrap();
    assert_eq!(decoded.sig[0][0], 100);
    assert_eq!(decoded.sig[0][1], -100);
  }
}
