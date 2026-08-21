//! Similarity scoring, replicating the C++ `queryFromSignature` exactly.
//!
//! Score arithmetic is f32 end-to-end and per-slot operation order matches the
//! C++ (channels ascending, coefficients in sorted-sig order), so scores are
//! bit-identical to the original, not merely close. Do not reorder loops or
//! "improve" the accumulation without re-verifying against the reference
//! implementation below and the differential harness.

// Indexed loops here deliberately mirror the C++ iteration structure that the
// parity guarantees are argued against.
#![allow(clippy::needless_range_loop)]

use rayon::prelude::*;

use super::{bucket_index, Chunk, Index};
use crate::signature::HaarSignature;
use crate::{NUM_COEFS, NUM_PIXELS};

/// The per-bin coefficient weights, verbatim from the paper via `imglib.h`.
pub const WEIGHTS: [[f32; 3]; 6] = [
  [5.00, 19.21, 34.37],
  [0.83, 1.26, 0.36],
  [1.01, 0.44, 0.45],
  [0.52, 0.53, 0.14],
  [0.47, 0.28, 0.18],
  [0.30, 0.14, 0.27],
];

/// Weight bin for a coefficient position: min(max(row, col), 5) over the
/// 128x128 matrix, i.e. the `imgBin` table without the table.
#[inline]
fn bin(idx: u16) -> usize {
  let row = (idx as usize) / NUM_PIXELS;
  let col = (idx as usize) % NUM_PIXELS;
  row.max(col).min(5)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Match {
  pub post_id: u32,
  pub score: f32,
}

/// Candidate ordering: by raw score ascending (lower = more similar), ties
/// broken toward the earlier slot – the observable consequence of the C++
/// priority queue's strict-`<` replacement rule.
#[derive(Debug, Clone, Copy)]
struct Cand {
  raw: f32,
  slot: u32,
}

impl PartialEq for Cand {
  fn eq(&self, other: &Self) -> bool {
    self.raw.to_bits() == other.raw.to_bits() && self.slot == other.slot
  }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    Some(self.cmp(other))
  }
}
impl Ord for Cand {
  fn cmp(&self, other: &Self) -> std::cmp::Ordering {
    self
      .raw
      .total_cmp(&other.raw)
      .then(self.slot.cmp(&other.slot))
  }
}

pub fn query(
  index: &Index,
  sig: &HaarSignature,
  limit: usize,
  min_score: Option<f32>,
) -> Vec<Match> {
  if limit == 0 || index.is_empty() {
    return Vec::new();
  }

  let colors = sig.num_colors();
  let qavgl: [f32; 3] = [
    sig.avglf[0] as f32,
    sig.avglf[1] as f32,
    sig.avglf[2] as f32,
  ];

  // Scale accumulates in C++ iteration order, skipping buckets that are
  // empty across the whole index.
  let counts = index.bucket_counts();
  let mut scale: f32 = 0.0;
  for c in 0..colors {
    for i in 0..NUM_COEFS {
      let coef = sig.sig[c][i];
      if counts[bucket_index(c, coef)] > 0 {
        scale -= WEIGHTS[bin(coef.unsigned_abs())][c];
      }
    }
  }

  // Per-chunk scoring + local top-K, in parallel. The score buffer is
  // per-worker scratch (map_init), NOT a fresh allocation per chunk: a
  // 256KB Vec is over glibc's mmap threshold, and allocating ~100 of them
  // per query causes mmap/munmap TLB-shootdown storms under concurrent
  // load (measured: p99 30x worse).
  let mut candidates: Vec<Cand> = index
    .chunks()
    .par_iter()
    .enumerate()
    .map_init(Vec::new, |scratch, (chunk_idx, chunk)| {
      query_chunk(chunk, sig, colors, &qavgl, limit, scratch)
        .into_iter()
        .map(|(raw, local)| Cand {
          raw,
          slot: ((chunk_idx as u32) << 16) | local as u32,
        })
        .collect::<Vec<_>>()
    })
    .flatten()
    .collect();

  candidates.sort_unstable();
  candidates.truncate(limit);

  if scale != 0.0 {
    scale = 1.0 / scale;
  }

  candidates
    .into_iter()
    .map(|cand| {
      let chunk = &index.chunks()[(cand.slot >> 16) as usize];
      Match {
        post_id: chunk.posts[(cand.slot & 0xffff) as usize],
        score: cand.raw * 100.0 * scale,
      }
    })
    .filter(|m| min_score.is_none_or(|min| m.score >= min))
    .collect()
}

/// Score one chunk and return its top-`limit` (raw_score, local_slot) pairs.
/// `scores` is reusable scratch owned by the calling worker.
fn query_chunk(
  chunk: &Chunk,
  sig: &HaarSignature,
  colors: usize,
  qavgl: &[f32; 3],
  limit: usize,
  scores: &mut Vec<f32>,
) -> Vec<(f32, u16)> {
  let n = chunk.len();
  if n == 0 {
    return Vec::new();
  }

  // Luminance pre-pass: identical accumulation order to the C++ (channel
  // ascending), computed for every slot; tombstones are filtered at
  // selection, matching the original's behavior.
  scores.clear();
  scores.reserve(n);
  for slot in 0..n {
    let mut s: f32 = 0.0;
    for c in 0..colors {
      s += WEIGHTS[0][c] * (chunk.avgl[c][slot] - qavgl[c]).abs();
    }
    scores.push(s);
  }

  // Bucket pass in (channel, sorted-coefficient) order. Each slot appears at
  // most once per bucket, so per-slot subtraction order is fully determined
  // by this outer loop – the f32 parity hinges on it.
  for c in 0..colors {
    for i in 0..NUM_COEFS {
      let coef = sig.sig[c][i];
      let bucket = &chunk.buckets[bucket_index(c, coef)];
      if bucket.is_empty() {
        continue;
      }
      let weight = WEIGHTS[bin(coef.unsigned_abs())][c];
      for &slot in bucket.as_slice() {
        scores[slot as usize] -= weight;
      }
    }
  }

  // Bounded selection: max-heap of the worst kept candidate; strict
  // improvement required to displace, earlier slot wins ties.
  let mut heap: std::collections::BinaryHeap<Cand> =
    std::collections::BinaryHeap::with_capacity(limit + 1);
  for (slot, &raw) in scores.iter().enumerate() {
    if chunk.is_deleted(slot as u16) {
      continue;
    }
    let cand = Cand {
      raw,
      slot: slot as u32,
    };
    if heap.len() < limit {
      heap.push(cand);
    } else if let Some(top) = heap.peek() {
      if cand < *top {
        heap.pop();
        heap.push(cand);
      }
    }
  }

  heap.into_iter().map(|c| (c.raw, c.slot as u16)).collect()
}

#[cfg(test)]
pub(crate) mod reference {
  //! A literal transcription of the C++ `IQDB` in-memory scoring
  //! (`imgdb.cpp`), used as the oracle for equivalence testing. One flat
  //! image array, one global bucket set, append-ordered buckets, tombstone
  //! encoded as an explicit flag (standing in for the C++ `avgl[0] == 0`
  //! convention, equivalent whenever avglf[0] != 0).

  use super::*;
  use std::collections::HashMap;

  struct Info {
    post_id: u32,
    avgl: [f32; 3],
    deleted: bool,
  }

  #[derive(Default)]
  pub struct NaiveIndex {
    infos: Vec<Info>,
    sigs: Vec<HaarSignature>,
    buckets: HashMap<usize, Vec<u32>>, // bucket -> internal ids, append order
    by_post: HashMap<u32, u32>,
  }

  impl NaiveIndex {
    pub fn new() -> Self {
      Self::default()
    }

    pub fn add(&mut self, post_id: u32, sig: HaarSignature) {
      self.remove(post_id); // C++ addImage calls removeImage first
      let id = self.infos.len() as u32;
      for c in 0..sig.num_colors() {
        for &coef in &sig.sig[c] {
          self
            .buckets
            .entry(bucket_index(c, coef))
            .or_default()
            .push(id);
        }
      }
      self.infos.push(Info {
        post_id,
        avgl: [
          sig.avglf[0] as f32,
          sig.avglf[1] as f32,
          sig.avglf[2] as f32,
        ],
        deleted: false,
      });
      self.sigs.push(sig);
      self.by_post.insert(post_id, id);
    }

    pub fn remove(&mut self, post_id: u32) {
      let Some(id) = self.by_post.remove(&post_id) else {
        return;
      };
      let sig = self.sigs[id as usize].clone();
      for c in 0..sig.num_colors() {
        for &coef in &sig.sig[c] {
          if let Some(bucket) = self.buckets.get_mut(&bucket_index(c, coef)) {
            bucket.retain(|&x| x != id); // erase-remove idiom
          }
        }
      }
      self.infos[id as usize].deleted = true;
    }

    /// `queryFromSignature`, transcribed.
    pub fn query(&self, sig: &HaarSignature, numres: usize) -> Vec<Match> {
      let colors = sig.num_colors();
      let qavgl = [
        sig.avglf[0] as f32,
        sig.avglf[1] as f32,
        sig.avglf[2] as f32,
      ];

      let mut scale: f32 = 0.0;
      let mut scores: Vec<f32> = vec![0.0; self.infos.len()];

      for (i, info) in self.infos.iter().enumerate() {
        let mut s: f32 = 0.0;
        for c in 0..colors {
          s += WEIGHTS[0][c] * (info.avgl[c] - qavgl[c]).abs();
        }
        scores[i] = s;
      }

      for c in 0..colors {
        for i in 0..NUM_COEFS {
          let coef = sig.sig[c][i];
          let Some(bucket) = self.buckets.get(&bucket_index(c, coef)) else {
            continue;
          };
          if bucket.is_empty() {
            continue;
          }
          let weight = WEIGHTS[bin(coef.unsigned_abs())][c];
          scale -= weight;
          for &id in bucket {
            scores[id as usize] -= weight;
          }
        }
      }

      // Selection with the same tie policy as the chunked engine:
      // (score, internal id) ascending, keep the best `numres`.
      let mut cands: Vec<Cand> = scores
        .iter()
        .enumerate()
        .filter(|&(i, _)| !self.infos[i].deleted)
        .map(|(i, &raw)| Cand {
          raw,
          slot: i as u32,
        })
        .collect();
      cands.sort_unstable();
      cands.truncate(numres);

      if scale != 0.0 {
        scale = 1.0 / scale;
      }

      cands
        .into_iter()
        .map(|c| Match {
          post_id: self.infos[c.slot as usize].post_id,
          score: c.raw * 100.0 * scale,
        })
        .collect()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::reference::NaiveIndex;
  use super::*;
  use crate::signature::HaarSignature;
  use proptest::prelude::*;

  /// Random valid signature: distinct signed coefficient positions per
  /// channel, |coef| in 1..=16383, avglf[0] != 0 (the pure-black divergence
  /// is intentional and excluded here).
  fn arb_sig() -> impl Strategy<Value = HaarSignature> {
    (
      any::<u32>(),
      0.001f64..2.0,
      -0.2f64..0.2,
      -0.2f64..0.2,
      any::<bool>(),
    )
      .prop_map(|(seed, y, i_, q, grayscale)| {
        let mut sig = crate::index::tests::test_sig(seed, false).sig;
        for channel in &mut sig {
          channel.sort_unstable();
        }
        let avglf = if grayscale {
          [y, i_ * 0.01, q * 0.01]
        } else {
          [y, 0.05 + i_.abs(), 0.05 + q.abs()]
        };
        HaarSignature { avglf, sig }
      })
  }

  #[derive(Debug, Clone)]
  #[allow(clippy::large_enum_variant)] // test-only op stream
  enum Op {
    Add(u32, HaarSignature),
    Remove(u32),
  }

  fn arb_ops(max_posts: u32) -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(
      prop_oneof![
          5 => (0..max_posts, arb_sig()).prop_map(|(id, sig)| Op::Add(id, sig)),
          1 => (0..max_posts).prop_map(Op::Remove),
      ],
      1..60,
    )
  }

  proptest! {
      #![proptest_config(ProptestConfig::with_cases(64))]
      #[test]
      fn chunked_matches_reference_bit_for_bit(
          ops in arb_ops(40),
          queries in proptest::collection::vec(arb_sig(), 1..4),
          limit in 1usize..12,
      ) {
          let mut index = Index::new();
          let mut naive = NaiveIndex::new();
          for op in ops {
              match op {
                  Op::Add(id, sig) => {
                      index.insert(id, sig.clone());
                      naive.add(id, sig);
                  }
                  Op::Remove(id) => {
                      index.remove(id);
                      naive.remove(id);
                  }
              }
          }
          index.check_invariants();
          for q in &queries {
              let got = query(&index, q, limit, None);
              let expected = naive.query(q, limit);
              prop_assert_eq!(got.len(), expected.len());
              for (g, e) in got.iter().zip(expected.iter()) {
                  prop_assert_eq!(g.post_id, e.post_id);
                  prop_assert_eq!(
                      g.score.to_bits(), e.score.to_bits(),
                      "score mismatch for post {}: {} vs {}", g.post_id, g.score, e.score
                  );
              }
          }
      }
  }

  #[test]
  fn crosses_chunk_boundary_matches_reference() {
    // Spill into a second chunk (65,536 slots per chunk) and verify the
    // fan-out/merge path against the flat reference, including tombstones.
    let mut index = Index::bulk_build(
      (0..66_000u32).map(|i| (i, crate::index::tests::test_sig(i % 997, i % 11 == 0))),
    );
    let mut naive = NaiveIndex::new();
    for i in 0..66_000u32 {
      naive.add(i, crate::index::tests::test_sig(i % 997, i % 11 == 0));
    }
    for i in (0..66_000u32).step_by(1000) {
      index.remove(i);
      naive.remove(i);
    }
    let q = crate::index::tests::test_sig(123, false);
    let got = query(&index, &q, 20, None);
    let expected = naive.query(&q, 20);
    assert_eq!(got.len(), expected.len());
    for (g, e) in got.iter().zip(expected.iter()) {
      assert_eq!(g.post_id, e.post_id);
      assert_eq!(g.score.to_bits(), e.score.to_bits());
    }
  }

  #[test]
  fn bulk_build_matches_reference() {
    let rows: Vec<(u32, HaarSignature)> = (0..300u32)
      .map(|i| {
        let mut sig = crate::index::tests::test_sig(i, i % 5 == 0);
        sig.avglf[0] = 0.3 + (i as f64) * 1e-4;
        (i, sig)
      })
      .collect();
    let index = Index::bulk_build(rows.clone());
    let mut naive = NaiveIndex::new();
    for (id, sig) in rows {
      naive.add(id, sig);
    }
    let q = crate::index::tests::test_sig(7, false);
    let got = query(&index, &q, 15, None);
    let expected = naive.query(&q, 15);
    assert_eq!(got.len(), expected.len());
    for (g, e) in got.iter().zip(expected.iter()) {
      assert_eq!(g.post_id, e.post_id);
      assert_eq!(g.score.to_bits(), e.score.to_bits());
    }
  }

  #[test]
  fn self_query_scores_100() {
    let mut index = Index::new();
    let sig = crate::index::tests::test_sig(42, false);
    index.insert(1, sig.clone());
    index.insert(2, crate::index::tests::test_sig(43, false));
    let results = query(&index, &sig, 10, None);
    assert_eq!(results[0].post_id, 1);
    // An exact self-match: raw = luminance 0 minus every weight = scale,
    // so score = scale * 100 / scale = 100.
    assert!(
      (results[0].score - 100.0).abs() < 1e-3,
      "score = {}",
      results[0].score
    );
    assert!(results[1].score < results[0].score);
  }

  #[test]
  fn grayscale_query_uses_only_channel_zero() {
    let mut index = Index::new();
    let gray = crate::index::tests::test_sig(1, true);
    assert!(gray.is_grayscale());
    index.insert(1, gray.clone());
    let results = query(&index, &gray, 5, None);
    assert!((results[0].score - 100.0).abs() < 1e-3);

    // A color query never sees grayscale bucket entries in channels 1-2,
    // but the image still gets luminance-scored; parity with reference.
    let color = crate::index::tests::test_sig(2, false);
    let mut naive = NaiveIndex::new();
    naive.add(1, gray);
    let got = query(&index, &color, 5, None);
    let expected = naive.query(&color, 5);
    assert_eq!(got.len(), expected.len());
    assert_eq!(got[0].score.to_bits(), expected[0].score.to_bits());
  }

  #[test]
  fn min_score_filters() {
    let mut index = Index::new();
    index.insert(1, crate::index::tests::test_sig(42, false));
    index.insert(2, crate::index::tests::test_sig(999, false));
    let sig = crate::index::tests::test_sig(42, false);
    let all = query(&index, &sig, 10, None);
    assert_eq!(all.len(), 2);
    let cut = query(&index, &sig, 10, Some(90.0));
    assert_eq!(cut.len(), 1);
    assert_eq!(cut[0].post_id, 1);
  }

  #[test]
  fn limit_zero_and_empty_index() {
    let index = Index::new();
    let sig = crate::index::tests::test_sig(1, false);
    assert!(query(&index, &sig, 10, None).is_empty());
    let mut index = Index::new();
    index.insert(1, sig.clone());
    assert!(query(&index, &sig, 0, None).is_empty());
  }
}
