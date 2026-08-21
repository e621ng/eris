//! The Haar wavelet transform and coefficient selection.
//!
//! The math in this module is FROZEN: it must produce signatures identical to
//! the C++ IQDB (`haar.cpp`) that indexed the production database. Do not
//! "fix" the 0.7071 constant, the scaling scheme, or the selection rules.
//!
//! Adapted from iqdb-rs by TheBobBobs (https://github.com/TheBobBobs/iqdb-rs,
//! GPL-2.0), itself a port of the imgSeek/IQDB implementation by Ricardo
//! Niederberger Cabral and piespy (GPL-2.0). The image-decoding entry points
//! of the original are intentionally not included: ERIS only accepts
//! pre-decoded 128x128 channel data, so exactly one resampler (the client's)
//! ever produces signatures.

// Indexed loops here deliberately mirror the C++ iteration structure that the
// parity guarantees are argued against.
#![allow(clippy::needless_range_loop)]

use crate::signature::HaarSignature;
use crate::{NUM_COEFS, NUM_PIXELS, NUM_PIXELS_SQUARED};

/// Compute a signature from raw 128x128 R/G/B channel data (row-major,
/// values 0..=255). Equivalent to the C++ `transformChar` + `calcHaar` +
/// per-channel sort performed by `HaarSignature::from_channels`.
///
/// Panics if any slice is not exactly `NUM_PIXELS_SQUARED` long; callers
/// validate input lengths at the API boundary.
pub fn from_channels(r: &[u8], g: &[u8], b: &[u8]) -> HaarSignature {
  assert_eq!(r.len(), NUM_PIXELS_SQUARED);
  assert_eq!(g.len(), NUM_PIXELS_SQUARED);
  assert_eq!(b.len(), NUM_PIXELS_SQUARED);

  let mut a: Vec<f64> = r.iter().map(|&v| v as f64).collect();
  let mut bb: Vec<f64> = g.iter().map(|&v| v as f64).collect();
  let mut c: Vec<f64> = b.iter().map(|&v| v as f64).collect();

  transform(&mut a, &mut bb, &mut c);

  let avglf = [a[0], bb[0], c[0]];
  let mut sig = [get_m_largest(&a), get_m_largest(&bb), get_m_largest(&c)];
  for channel in &mut sig {
    channel.sort_unstable();
  }

  HaarSignature { avglf, sig }
}

/// RGB -> YIQ colorspace conversion (NTSC FCC matrix, exactly as `haar.cpp`).
fn rgb_to_yiq(r: &mut [f64], g: &mut [f64], b: &mut [f64]) {
  for i in 0..NUM_PIXELS_SQUARED {
    let y = 0.299 * r[i] + 0.587 * g[i] + 0.114 * b[i];
    let i_ = 0.596 * r[i] - 0.275 * g[i] - 0.321 * b[i];
    let q = 0.212 * r[i] - 0.523 * g[i] + 0.311 * b[i];
    r[i] = y;
    g[i] = i_;
    b[i] = q;
  }
}

fn transform(r: &mut [f64], g: &mut [f64], b: &mut [f64]) {
  rgb_to_yiq(r, g, b);

  haar_2d(r);
  haar_2d(g);
  haar_2d(b);

  // Reintroduce the skipped DC scaling factor.
  r[0] /= 256.0 * 128.0;
  g[0] /= 256.0 * 128.0;
  b[0] /= 256.0 * 128.0;
}

/// In-place 2D Haar tensorial transform. Only differences are scaled per
/// level (constant propagation scheme from the original); non-DC magnitudes
/// are therefore non-canonical, which is fine: only their ordering matters.
#[allow(clippy::approx_constant)] // 0.7071 is load-bearing; do not use FRAC_1_SQRT_2
fn haar_2d(a: &mut [f64]) {
  let mut temp = [0.0; NUM_PIXELS >> 1];

  // Decompose rows.
  let mut i = 0;
  while i < NUM_PIXELS_SQUARED {
    let mut c = 1.0;
    let mut h = NUM_PIXELS;
    while h > 1 {
      let h1 = h >> 1;
      c *= 0.7071;

      let mut k = 0;
      let mut j1 = i;
      let mut j2 = i;
      while k < h1 {
        let j21 = j2 + 1;
        temp[k] = (a[j2] - a[j21]) * c;
        a[j1] = a[j2] + a[j21];
        k += 1;
        j1 += 1;
        j2 += 2;
      }
      a[i + h1..i + h1 + h1].copy_from_slice(&temp[..h1]);
      h = h1;
    }
    a[i] *= c;
    i += NUM_PIXELS;
  }

  // Decompose columns.
  for i in 0..NUM_PIXELS {
    let mut c = 1.0;
    let mut h = NUM_PIXELS;
    while h > 1 {
      let h1 = h >> 1;
      c *= 0.7071;

      let mut k = 0;
      let mut j1 = i;
      let mut j2 = i;
      while k < h1 {
        let j21 = j2 + NUM_PIXELS;
        temp[k] = (a[j2] - a[j21]) * c;
        a[j1] = a[j2] + a[j21];
        k += 1;
        j1 += NUM_PIXELS;
        j2 += 2 * NUM_PIXELS;
      }
      let mut k = 0;
      let mut j1 = i + h1 * NUM_PIXELS;
      while k < h1 {
        a[j1] = temp[k];
        k += 1;
        j1 += NUM_PIXELS;
      }
      h = h1;
    }
    a[i] *= c;
  }
}

/// Select the NUM_COEFS largest-magnitude coefficients, excluding the DC term
/// at index 0. The returned values are the coefficient *indices*, negated when
/// the coefficient was <= 0.0 (exactly-zero coefficients get a negative index,
/// matching the C++ `cdata[i] > 0` test).
fn get_m_largest(data: &[f64]) -> [i16; NUM_COEFS] {
  struct V {
    i: usize,
    d: f64,
  }
  impl PartialEq for V {
    fn eq(&self, other: &Self) -> bool {
      self.d == other.d
    }
  }
  impl Eq for V {}
  impl PartialOrd for V {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
      Some(self.cmp(other))
    }
  }
  impl Ord for V {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
      // Reversed: BinaryHeap is a max-heap, we want the smallest on top.
      self.d.total_cmp(&other.d).reverse()
    }
  }

  let mut heap = std::collections::BinaryHeap::with_capacity(NUM_COEFS);
  for i in 1..NUM_COEFS + 1 {
    heap.push(V {
      i,
      d: data[i].abs(),
    });
  }
  for i in NUM_COEFS + 1..NUM_PIXELS_SQUARED {
    let d = data[i].abs();
    // Strict >, as in the C++: equal magnitudes never displace an entry.
    if d > heap.peek().expect("heap is non-empty").d {
      heap.pop();
      heap.push(V { i, d });
    }
  }

  let mut sig = [0i16; NUM_COEFS];
  let mut cnt = 0;
  while let Some(value) = heap.pop() {
    let mut c = value.i as i16;
    if data[value.i] <= 0.0 {
      c = -c;
    }
    sig[cnt] = c;
    cnt += 1;
  }
  debug_assert_eq!(cnt, NUM_COEFS);
  sig
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pure_black_has_zero_avgl_and_negative_low_indices() {
    let zeros = vec![0u8; NUM_PIXELS_SQUARED];
    let sig = from_channels(&zeros, &zeros, &zeros);
    assert_eq!(sig.avglf, [0.0, 0.0, 0.0]);
    // All coefficients are 0.0 -> the seeded indices 1..=40 survive,
    // negated (<= 0.0 rule), then sorted ascending.
    for c in 0..3 {
      let expected: Vec<i16> = (1..=40).rev().map(|i| -(i as i16)).collect();
      assert_eq!(sig.sig[c].to_vec(), expected);
    }
  }

  #[test]
  fn flat_white_avgl_matches_dc_scaling() {
    let white = vec![255u8; NUM_PIXELS_SQUARED];
    let sig = from_channels(&white, &white, &white);
    // For a flat image the DC works out to mean/256 (up to the 0.7071^14
    // vs 2^-7 rounding drift): 255/256 * (0.7071^14 * 128) ~= 0.9959.
    assert!(
      (sig.avglf[0] - 0.9959).abs() < 0.001,
      "avglf[0] = {}",
      sig.avglf[0]
    );
    assert!(sig.avglf[1].abs() < 1e-9);
    assert!(sig.avglf[2].abs() < 1e-9);
    assert!(sig.is_grayscale(), "flat white must be grayscale");
  }

  #[test]
  fn channels_are_sorted_ascending() {
    let mut r = vec![0u8; NUM_PIXELS_SQUARED];
    for (i, v) in r.iter_mut().enumerate() {
      *v = (i % 251) as u8;
    }
    let g = r.clone();
    let b: Vec<u8> = r.iter().map(|&v| 255 - v).collect();
    let sig = from_channels(&r, &g, &b);
    for c in 0..3 {
      assert!(sig.sig[c].windows(2).all(|w| w[0] < w[1]));
    }
  }

  #[test]
  fn deterministic() {
    let mut r = vec![0u8; NUM_PIXELS_SQUARED];
    let mut state = 0x12345678u32;
    for v in r.iter_mut() {
      state = state.wrapping_mul(1664525).wrapping_add(1013904223);
      *v = (state >> 24) as u8;
    }
    let g: Vec<u8> = r.iter().map(|&v| v.wrapping_add(17)).collect();
    let b: Vec<u8> = r.iter().map(|&v| v ^ 0x5a).collect();
    assert_eq!(from_channels(&r, &g, &b), from_channels(&r, &g, &b));
  }
}
