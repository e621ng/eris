/// A minimal fixed-purpose bitset for tombstone tracking. Bits are only ever
/// set – tombstoned slots are never reused; compaction is a re-bootstrap.
#[derive(Clone, Debug, Default)]
pub struct BitSet {
  words: Vec<u64>,
}

impl BitSet {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn set(&mut self, i: usize) {
    let word = i / 64;
    if word >= self.words.len() {
      self.words.resize(word + 1, 0);
    }
    self.words[word] |= 1u64 << (i % 64);
  }

  pub fn get(&self, i: usize) -> bool {
    self
      .words
      .get(i / 64)
      .is_some_and(|w| w & (1u64 << (i % 64)) != 0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn set_and_get() {
    let mut b = BitSet::new();
    assert!(!b.get(0));
    assert!(!b.get(1000));
    b.set(63);
    b.set(64);
    b.set(64); // idempotent
    assert!(b.get(63) && b.get(64) && !b.get(65));
  }
}
