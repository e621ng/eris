// Indexed loops keep the channel/coefficient layout explicit.
#![allow(clippy::needless_range_loop)]

use crate::{NUM_COEFS, NUM_PIXELS_SQUARED};

/// Size in bytes of the coefficient blob as stored on disk: 3 channels x 40
/// little-endian i16 values.
pub const SIG_BLOB_LEN: usize = 3 * NUM_COEFS * 2;

/// A Haar wavelet signature, identical in layout and semantics to the C++
/// `HaarSignature` (avglf: YIQ DC components; sig: the 40 strongest
/// coefficient indices per channel, sign-encoded, sorted ascending when
/// produced by `from_channels`).
#[derive(Clone, Debug, PartialEq)]
pub struct HaarSignature {
  pub avglf: [f64; 3],
  pub sig: [[i16; NUM_COEFS]; 3],
}

#[derive(Debug, PartialEq, Eq)]
pub enum SigError {
  /// The blob was not exactly 240 bytes.
  BadBlobLength(usize),
  /// A coefficient index was outside +-(NUM_PIXELS_SQUARED - 1).
  CoefOutOfRange(i16),
}

impl std::fmt::Display for SigError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      SigError::BadBlobLength(n) => {
        write!(f, "signature blob is {n} bytes, expected {SIG_BLOB_LEN}")
      }
      SigError::CoefOutOfRange(c) => write!(f, "coefficient index {c} out of range"),
    }
  }
}

impl std::error::Error for SigError {}

impl HaarSignature {
  /// Grayscale detection, exactly as the C++ (`|avglf[1]| + |avglf[2]| <
  /// 6.0/1000`, strict). Grayscale signatures index into and query only
  /// channel 0.
  pub fn is_grayscale(&self) -> bool {
    self.avglf[1].abs() + self.avglf[2].abs() < 6.0 / 1000.0
  }

  pub fn num_colors(&self) -> usize {
    if self.is_grayscale() {
      1
    } else {
      3
    }
  }

  /// Decode from the storage representation (three doubles plus the 240-byte
  /// little-endian i16 blob). Does NOT re-sort: stored blobs are already
  /// sorted, and parity with the C++ (`Image::haar()` reinterprets the raw
  /// bytes) requires taking them as-is.
  pub fn from_blob(avglf: [f64; 3], blob: &[u8]) -> Result<Self, SigError> {
    if blob.len() != SIG_BLOB_LEN {
      return Err(SigError::BadBlobLength(blob.len()));
    }
    let mut sig = [[0i16; NUM_COEFS]; 3];
    for (i, pair) in blob.chunks_exact(2).enumerate() {
      let coef = i16::from_le_bytes([pair[0], pair[1]]);
      if coef.unsigned_abs() as usize >= NUM_PIXELS_SQUARED {
        return Err(SigError::CoefOutOfRange(coef));
      }
      sig[i / NUM_COEFS][i % NUM_COEFS] = coef;
    }
    Ok(HaarSignature { avglf, sig })
  }

  /// Encode the coefficients as the 240-byte little-endian blob.
  pub fn sig_blob(&self) -> [u8; SIG_BLOB_LEN] {
    let mut out = [0u8; SIG_BLOB_LEN];
    for c in 0..3 {
      for i in 0..NUM_COEFS {
        let bytes = self.sig[c][i].to_le_bytes();
        let off = (c * NUM_COEFS + i) * 2;
        out[off] = bytes[0];
        out[off + 1] = bytes[1];
      }
    }
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> HaarSignature {
    let mut sig = [[0i16; NUM_COEFS]; 3];
    for c in 0..3 {
      for i in 0..NUM_COEFS {
        let v = (c as i16 + 1) * (i as i16 + 1);
        sig[c][i] = if i % 2 == 0 { v } else { -v };
      }
    }
    HaarSignature {
      avglf: [0.5, -0.001, 0.25],
      sig,
    }
  }

  #[test]
  fn blob_round_trip() {
    let s = sample();
    let blob = s.sig_blob();
    let back = HaarSignature::from_blob(s.avglf, &blob).unwrap();
    assert_eq!(s, back);
  }

  #[test]
  fn blob_length_checked() {
    assert_eq!(
      HaarSignature::from_blob([0.0; 3], &[0u8; 239]),
      Err(SigError::BadBlobLength(239))
    );
  }

  #[test]
  fn coef_range_checked() {
    let mut blob = [0u8; SIG_BLOB_LEN];
    // 16384 is out of range (valid indices are 0..=16383 in magnitude).
    blob[0..2].copy_from_slice(&16384i16.to_le_bytes());
    assert!(HaarSignature::from_blob([0.0; 3], &blob).is_err());
  }

  #[test]
  fn grayscale_threshold_is_strict() {
    let mut s = sample();
    s.avglf = [0.5, 0.003, 0.0029999];
    assert!(s.is_grayscale());
    s.avglf = [0.5, 0.003, 0.003]; // sum == 0.006 exactly: NOT grayscale
    assert!(!s.is_grayscale());
    s.avglf = [0.5, -0.004, 0.0021];
    assert!(!s.is_grayscale());
  }
}
