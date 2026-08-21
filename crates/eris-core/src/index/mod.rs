//! The in-memory search index: chunked, struct-of-arrays, with explicit
//! tombstones and a global bucket census for C++-parity scale computation.

mod bitset;
mod bucket;
mod chunk;
pub mod query;

use std::collections::HashMap;

use crate::signature::HaarSignature;

pub(crate) use chunk::{bucket_index, Chunk, BUCKETS_PER_CHUNK};

#[derive(Debug, Clone, PartialEq)]
pub struct IndexStats {
  pub live: u64,
  pub tombstones: u64,
  pub chunks: usize,
  pub heap_bytes: usize,
}

pub struct Index {
  chunks: Vec<Chunk>,
  /// post_id -> global slot (chunk << 16 | local).
  post_to_slot: HashMap<u32, u32>,
  /// Live entry count per bucket position across all chunks. The scoring
  /// scale must skip buckets that are empty *globally* (C++ parity), which
  /// per-chunk emptiness cannot answer.
  bucket_counts: Vec<u32>,
  tombstones: u64,
}

impl Default for Index {
  fn default() -> Self {
    Self::new()
  }
}

impl Index {
  pub fn new() -> Self {
    Index {
      chunks: Vec::new(),
      post_to_slot: HashMap::new(),
      bucket_counts: vec![0; BUCKETS_PER_CHUNK],
      tombstones: 0,
    }
  }

  /// Build from an iterator of (post_id, signature). For deterministic slot
  /// assignment (and therefore C++-comparable tie behavior) feed rows in
  /// post_id-ascending order. Much faster than repeated `insert`: bucket
  /// entries are appended unsorted and sorted once at the end.
  pub fn bulk_build(rows: impl IntoIterator<Item = (u32, HaarSignature)>) -> Self {
    let mut index = Index::new();
    for (post_id, sig) in rows {
      index.append(post_id, sig, false);
    }
    index.finalize();
    index
  }

  /// Restore bucket invariants after unsorted bulk appends. Idempotent.
  pub fn finalize(&mut self) {
    use rayon::prelude::*;
    self
      .chunks
      .par_iter_mut()
      .for_each(|chunk| chunk.finalize());
  }

  /// Insert or replace. A replace tombstones the old slot and appends a
  /// fresh one, mirroring the C++ (remove + new rowid); slots are never
  /// reused, so long-running replace churn shows up as tombstone growth.
  pub fn insert(&mut self, post_id: u32, sig: HaarSignature) {
    self.remove(post_id);
    self.append(post_id, sig, true);
  }

  fn append(&mut self, post_id: u32, sig: HaarSignature, sorted: bool) {
    debug_assert!(
      !self.post_to_slot.contains_key(&post_id),
      "append of already-present post {post_id}"
    );
    if self.chunks.last().is_none_or(|c| c.is_full()) {
      self.chunks.push(Chunk::new());
    }
    for c in 0..sig.num_colors() {
      for &coef in &sig.sig[c] {
        self.bucket_counts[bucket_index(c, coef)] += 1;
      }
    }
    let chunk_idx = self.chunks.len() - 1;
    let local = self.chunks[chunk_idx].append(post_id, sig, sorted);
    self
      .post_to_slot
      .insert(post_id, ((chunk_idx as u32) << 16) | local as u32);
  }

  /// Remove a post. Returns false if it was not indexed.
  pub fn remove(&mut self, post_id: u32) -> bool {
    let Some(slot) = self.post_to_slot.remove(&post_id) else {
      return false;
    };
    let chunk = &mut self.chunks[(slot >> 16) as usize];
    let local = (slot & 0xffff) as u16;
    {
      let sig = &chunk.sigs[local as usize];
      for c in 0..sig.num_colors() {
        for &coef in &sig.sig[c] {
          self.bucket_counts[bucket_index(c, coef)] -= 1;
        }
      }
    }
    chunk.remove(local);
    self.tombstones += 1;
    true
  }

  pub fn get(&self, post_id: u32) -> Option<&HaarSignature> {
    let slot = *self.post_to_slot.get(&post_id)?;
    Some(&self.chunks[(slot >> 16) as usize].sigs[(slot & 0xffff) as usize])
  }

  pub fn contains(&self, post_id: u32) -> bool {
    self.post_to_slot.contains_key(&post_id)
  }

  pub fn len(&self) -> usize {
    self.post_to_slot.len()
  }

  pub fn is_empty(&self) -> bool {
    self.post_to_slot.is_empty()
  }

  pub fn stats(&self) -> IndexStats {
    IndexStats {
      live: self.post_to_slot.len() as u64,
      tombstones: self.tombstones,
      chunks: self.chunks.len(),
      heap_bytes: self.chunks.iter().map(|c| c.heap_bytes()).sum::<usize>()
        + self.bucket_counts.capacity() * 4
        + self.post_to_slot.capacity() * 16,
    }
  }

  pub(crate) fn chunks(&self) -> &[Chunk] {
    &self.chunks
  }

  pub(crate) fn bucket_counts(&self) -> &[u32] {
    &self.bucket_counts
  }

  /// Exhaustive consistency check for tests: bucket membership, counts, and
  /// tombstone bookkeeping all agree with the stored signatures.
  #[cfg(test)]
  pub(crate) fn check_invariants(&self) {
    let mut counts = vec![0u32; BUCKETS_PER_CHUNK];
    let mut live = 0usize;
    let mut dead = 0usize;
    for (ci, chunk) in self.chunks.iter().enumerate() {
      for local in 0..chunk.len() {
        let sig = &chunk.sigs[local];
        let deleted = chunk.is_deleted(local as u16);
        if deleted {
          dead += 1;
        } else {
          live += 1;
          let slot = ((ci as u32) << 16) | local as u32;
          assert_eq!(self.post_to_slot.get(&chunk.posts[local]), Some(&slot));
        }
        for c in 0..sig.num_colors() {
          for &coef in &sig.sig[c] {
            let idx = bucket_index(c, coef);
            let present = chunk.buckets[idx]
              .as_slice()
              .binary_search(&(local as u16))
              .is_ok();
            assert_eq!(present, !deleted, "bucket membership mismatch");
            if !deleted {
              counts[idx] += 1;
            }
          }
        }
      }
    }
    assert_eq!(counts, self.bucket_counts);
    assert_eq!(live, self.post_to_slot.len());
    assert_eq!(dead as u64, self.tombstones);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::NUM_COEFS;

  pub(crate) fn test_sig(seed: u32, grayscale: bool) -> HaarSignature {
    // Deterministic distinct coefficient indices per channel.
    let mut sig = [[0i16; NUM_COEFS]; 3];
    let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
    for channel in sig.iter_mut() {
      let mut used = std::collections::BTreeSet::new();
      let mut i = 0;
      while i < NUM_COEFS {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let idx = 1 + ((state >> 16) % 16383) as i16;
        let value = if state & 1 == 0 { idx } else { -idx };
        if used.insert(value) {
          channel[i] = value;
          i += 1;
        }
      }
    }
    for channel in &mut sig {
      channel.sort_unstable();
    }
    let avglf = if grayscale {
      [0.5 + seed as f64 * 1e-6, 0.001, 0.0005]
    } else {
      [0.5 + seed as f64 * 1e-6, 0.05, -0.02]
    };
    HaarSignature { avglf, sig }
  }

  #[test]
  fn insert_get_remove_replace() {
    let mut index = Index::new();
    index.insert(10, test_sig(1, false));
    index.insert(20, test_sig(2, true));
    assert_eq!(index.len(), 2);
    assert_eq!(index.get(10), Some(&test_sig(1, false)));
    index.check_invariants();

    // Replace burns a slot.
    index.insert(10, test_sig(3, false));
    assert_eq!(index.len(), 2);
    assert_eq!(index.stats().tombstones, 1);
    assert_eq!(index.get(10), Some(&test_sig(3, false)));
    index.check_invariants();

    assert!(index.remove(20));
    assert!(!index.remove(20));
    assert!(!index.remove(999));
    assert_eq!(index.len(), 1);
    assert_eq!(index.stats().tombstones, 2);
    index.check_invariants();
  }

  #[test]
  fn bulk_build_matches_incremental() {
    let rows: Vec<(u32, HaarSignature)> = (0..500).map(|i| (i, test_sig(i, i % 7 == 0))).collect();
    let bulk = Index::bulk_build(rows.clone());
    let mut incremental = Index::new();
    for (id, sig) in rows {
      incremental.insert(id, sig);
    }
    bulk.check_invariants();
    incremental.check_invariants();
    assert_eq!(bulk.bucket_counts(), incremental.bucket_counts());
    assert_eq!(bulk.len(), incremental.len());
  }

  #[test]
  fn chunk_boundary() {
    // Cross the 65,536-slot boundary with tombstones in the mix.
    let mut index = Index::bulk_build((0..70_000u32).map(|i| (i, test_sig(i % 97, false))));
    assert_eq!(index.stats().chunks, 2);
    assert!(index.remove(65_535)); // last slot of chunk 0
    assert!(index.remove(65_536)); // first slot of chunk 1
    assert!(index.contains(65_534));
    assert!(index.contains(65_537));
    assert_eq!(index.len(), 69_998);
  }
}
