use super::bitset::BitSet;
use super::bucket::Bucket;
use crate::signature::HaarSignature;
use crate::NUM_PIXELS_SQUARED;

/// Slots per chunk; chunk-local ids are u16.
pub const CHUNK_SLOTS: usize = 1 << 16;
/// Buckets per chunk: 3 colors x 2 signs x 16384 coefficient positions.
pub const BUCKETS_PER_CHUNK: usize = 3 * 2 * NUM_PIXELS_SQUARED;

/// Bucket addressing, shared by chunks and the global bucket counts:
/// color-major, then sign, then |coefficient|.
#[inline]
pub(crate) fn bucket_index(color: usize, coef: i16) -> usize {
  color * (2 * NUM_PIXELS_SQUARED)
    + (coef < 0) as usize * NUM_PIXELS_SQUARED
    + coef.unsigned_abs() as usize
}

/// A fixed-capacity shard of the index. Struct-of-arrays layout: the
/// luminance pre-pass streams `avgl`, the bucket pass streams bucket slices,
/// and `sigs` serves hash/post-id lookups without touching Postgres.
pub(crate) struct Chunk {
  pub(crate) avgl: [Vec<f32>; 3],
  pub(crate) posts: Vec<u32>,
  pub(crate) sigs: Vec<HaarSignature>,
  pub(crate) buckets: Vec<Bucket>,
  pub(crate) tombstones: BitSet,
}

impl Chunk {
  pub fn new() -> Self {
    Chunk {
      avgl: [Vec::new(), Vec::new(), Vec::new()],
      posts: Vec::new(),
      sigs: Vec::new(),
      buckets: vec![Bucket::new(); BUCKETS_PER_CHUNK],
      tombstones: BitSet::new(),
    }
  }

  pub fn len(&self) -> usize {
    self.posts.len()
  }

  pub fn is_full(&self) -> bool {
    self.len() >= CHUNK_SLOTS
  }

  /// Append an image. `sorted` chooses between invariant-maintaining bucket
  /// inserts (runtime path) and unsorted pushes that require `finalize()`
  /// (bootstrap path). Grayscale signatures index only channel 0, exactly
  /// like the C++ `bucket_set::add`.
  pub fn append(&mut self, post_id: u32, sig: HaarSignature, sorted: bool) -> u16 {
    debug_assert!(!self.is_full());
    let local = self.len() as u16;
    for c in 0..3 {
      self.avgl[c].push(sig.avglf[c] as f32);
    }
    for c in 0..sig.num_colors() {
      for &coef in &sig.sig[c] {
        let bucket = &mut self.buckets[bucket_index(c, coef)];
        if sorted {
          bucket.insert(local);
        } else {
          bucket.push_unsorted(local);
        }
      }
    }
    self.posts.push(post_id);
    self.sigs.push(sig);
    local
  }

  /// Restore bucket invariants after bulk appends.
  pub fn finalize(&mut self) {
    for bucket in &mut self.buckets {
      if !bucket.is_empty() {
        bucket.finalize();
      }
    }
  }

  /// Tombstone a slot: pull it out of its buckets (using the channels it was
  /// indexed under) and mark it deleted. The slot is never reused.
  pub fn remove(&mut self, local: u16) {
    debug_assert!(!self.tombstones.get(local as usize));
    let sig = &self.sigs[local as usize];
    let colors = sig.num_colors();
    for c in 0..colors {
      for i in 0..crate::NUM_COEFS {
        let coef = self.sigs[local as usize].sig[c][i];
        let removed = self.buckets[bucket_index(c, coef)].remove(local);
        debug_assert!(removed);
      }
    }
    self.tombstones.set(local as usize);
  }

  pub fn is_deleted(&self, local: u16) -> bool {
    self.tombstones.get(local as usize)
  }

  /// Approximate heap usage, for stats/metrics.
  pub fn heap_bytes(&self) -> usize {
    let vecs = self.avgl.iter().map(|v| v.capacity() * 4).sum::<usize>()
      + self.posts.capacity() * 4
      + self.sigs.capacity() * std::mem::size_of::<HaarSignature>()
      + self.buckets.capacity() * std::mem::size_of::<Bucket>();
    let spill: usize = self.buckets.iter().map(|b| b.heap_bytes()).sum();
    vecs + spill
  }
}
