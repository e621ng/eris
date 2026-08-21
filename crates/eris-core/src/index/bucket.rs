use smallvec::SmallVec;

/// One inverted-index bucket: the chunk-local slot ids of every live image
/// whose signature contains this (color, sign, coefficient) triple.
///
/// Invariant: sorted ascending, no duplicates. An image contributes at most
/// one entry per bucket by construction (coefficient indices within a channel
/// are distinct), so the invariant is cheap to maintain.
///
/// Small buckets live inline; larger ones spill to the heap via SmallVec.
/// There is deliberately no sentinel-terminated fixed-array variant: that
/// design carried a silent-corruption bug in iqdb-rs.
#[derive(Clone, Debug, Default)]
pub struct Bucket(SmallVec<[u16; 8]>);

impl Bucket {
  pub fn new() -> Self {
    Self::default()
  }

  /// Insert keeping the sorted invariant. Returns false if already present.
  pub fn insert(&mut self, id: u16) -> bool {
    match self.0.binary_search(&id) {
      Ok(_) => false,
      Err(pos) => {
        self.0.insert(pos, id);
        true
      }
    }
  }

  /// Append without maintaining order; requires a later `finalize()`.
  /// Used only by the bulk-build path.
  pub fn push_unsorted(&mut self, id: u16) {
    self.0.push(id);
  }

  /// Restore the sorted invariant after bulk `push_unsorted` calls.
  pub fn finalize(&mut self) {
    self.0.sort_unstable();
    debug_assert!(self.0.windows(2).all(|w| w[0] < w[1]));
  }

  /// Remove by value. Returns false if absent.
  pub fn remove(&mut self, id: u16) -> bool {
    match self.0.binary_search(&id) {
      Ok(pos) => {
        self.0.remove(pos);
        true
      }
      Err(_) => false,
    }
  }

  pub fn as_slice(&self) -> &[u16] {
    &self.0
  }

  pub fn is_empty(&self) -> bool {
    self.0.is_empty()
  }

  /// Bytes used by any heap spill (for stats).
  pub fn heap_bytes(&self) -> usize {
    if self.0.spilled() {
      self.0.capacity() * std::mem::size_of::<u16>()
    } else {
      0
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;
  use std::collections::BTreeSet;

  #[derive(Debug, Clone)]
  enum Op {
    Insert(u16),
    Remove(u16),
  }

  fn arb_ops() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(
      prop_oneof![
        // Small id space so inserts/removes collide often and the
        // bucket crosses the inline/spill boundary repeatedly.
        (0u16..32).prop_map(Op::Insert),
        (0u16..32).prop_map(Op::Remove),
      ],
      0..200,
    )
  }

  proptest! {
      #[test]
      fn matches_btreeset_model(ops in arb_ops()) {
          let mut bucket = Bucket::new();
          let mut model: BTreeSet<u16> = BTreeSet::new();
          for op in ops {
              match op {
                  Op::Insert(id) => {
                      prop_assert_eq!(bucket.insert(id), model.insert(id));
                  }
                  Op::Remove(id) => {
                      prop_assert_eq!(bucket.remove(id), model.remove(&id));
                  }
              }
              let expect: Vec<u16> = model.iter().copied().collect();
              prop_assert_eq!(bucket.as_slice(), expect.as_slice());
          }
      }

      #[test]
      fn bulk_build_equals_incremental(mut ids in proptest::collection::vec(any::<u16>(), 0..100)) {
          let mut bulk = Bucket::new();
          let mut incremental = Bucket::new();
          ids.sort_unstable();
          ids.dedup();
          // Feed in a shuffled-ish order (reversed) to exercise sorting.
          for &id in ids.iter().rev() {
              bulk.push_unsorted(id);
              incremental.insert(id);
          }
          bulk.finalize();
          assert_eq!(bulk.as_slice(), incremental.as_slice());
      }
  }
}
