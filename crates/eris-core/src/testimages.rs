//! Deterministic synthetic 128x128 channel data, shared by the golden-fixture
//! generator (which feeds these to the C++ oracle) and eris-core's golden
//! tests (which verify `from_channels` against the recorded oracle hashes).
//! Generation must stay byte-stable forever; add cases, never change them.

use crate::NUM_PIXELS_SQUARED;

pub const NUM_CASES: u32 = 30;

/// Generate case `n` (0..NUM_CASES). Panics on out-of-range.
pub fn case(n: u32) -> [Vec<u8>; 3] {
  let flat = |r: u8, g: u8, b: u8| {
    [
      vec![r; NUM_PIXELS_SQUARED],
      vec![g; NUM_PIXELS_SQUARED],
      vec![b; NUM_PIXELS_SQUARED],
    ]
  };
  let by_pixel = |f: &dyn Fn(usize, usize) -> (u8, u8, u8)| {
    let mut r = Vec::with_capacity(NUM_PIXELS_SQUARED);
    let mut g = Vec::with_capacity(NUM_PIXELS_SQUARED);
    let mut b = Vec::with_capacity(NUM_PIXELS_SQUARED);
    for y in 0..128 {
      for x in 0..128 {
        let (pr, pg, pb) = f(x, y);
        r.push(pr);
        g.push(pg);
        b.push(pb);
      }
    }
    [r, g, b]
  };
  let noise = |seed: u32, grayscale: bool| {
    let mut state = seed.wrapping_mul(2654435761).wrapping_add(12345);
    let mut step = move || {
      state = state.wrapping_mul(1664525).wrapping_add(1013904223);
      (state >> 24) as u8
    };
    let mut r = Vec::with_capacity(NUM_PIXELS_SQUARED);
    let mut g = Vec::with_capacity(NUM_PIXELS_SQUARED);
    let mut b = Vec::with_capacity(NUM_PIXELS_SQUARED);
    for _ in 0..NUM_PIXELS_SQUARED {
      let v = step();
      if grayscale {
        r.push(v);
        g.push(v);
        b.push(v);
      } else {
        r.push(v);
        g.push(step());
        b.push(step());
      }
    }
    [r, g, b]
  };

  match n {
    0 => flat(0, 0, 0),       // pure black: the avgl==0 edge case
    1 => flat(255, 255, 255), // pure white
    2 => flat(128, 128, 128), // mid gray
    3 => flat(255, 0, 0),
    4 => flat(0, 255, 0),
    5 => flat(0, 0, 255),
    6 => flat(1, 1, 1),                                             // near-black
    7 => by_pixel(&|x, _| (x as u8 * 2, x as u8 * 2, x as u8 * 2)), // h-gradient gray
    8 => by_pixel(&|_, y| (y as u8 * 2, y as u8 * 2, y as u8 * 2)), // v-gradient gray
    9 => by_pixel(&|x, y| (x as u8 * 2, y as u8 * 2, ((x + y) % 256) as u8)), // color gradient
    10 => by_pixel(&|x, y| {
      if (x / 8 + y / 8) % 2 == 0 {
        (255, 255, 255)
      } else {
        (0, 0, 0)
      }
    }),
    11 => by_pixel(&|x, y| {
      if (x / 16 + y / 16) % 2 == 0 {
        (200, 50, 50)
      } else {
        (50, 50, 200)
      }
    }),
    12 => by_pixel(&|x, y| {
      if (x / 2 + y / 2) % 2 == 0 {
        (255, 255, 255)
      } else {
        (0, 0, 0)
      }
    }), // fine checker
    13 => by_pixel(&|x, _| if x < 64 { (255, 255, 255) } else { (0, 0, 0) }), // half split
    14 => by_pixel(&|x, y| {
      // Centered disc.
      let dx = x as i32 - 64;
      let dy = y as i32 - 64;
      if dx * dx + dy * dy < 32 * 32 {
        (240, 240, 240)
      } else {
        (16, 16, 16)
      }
    }),
    // Grayscale-equal-channel noise (exercises is_grayscale).
    15 => noise(1, true),
    16 => noise(2, true),
    17 => noise(3, true),
    // Color noise.
    18 => noise(4, false),
    19 => noise(5, false),
    20 => noise(6, false),
    21 => noise(7, false),
    // Near the grayscale threshold: gray noise with a whisper of chroma.
    22 => {
      let [r, g, b] = noise(8, true);
      let r = r.iter().map(|&v| v.saturating_add(1)).collect();
      [r, g, b]
    }
    23 => {
      let [r, g, b] = noise(9, true);
      let b = b.iter().map(|&v| v.saturating_add(2)).collect();
      [r, g, b]
    }
    // Mostly-black noise (near-zero luminance).
    24 => {
      let [r, g, b] = noise(10, false);
      [
        r.iter().map(|&v| v / 32).collect(),
        g.iter().map(|&v| v / 32).collect(),
        b.iter().map(|&v| v / 32).collect(),
      ]
    }
    // Mostly-white.
    25 => {
      let [r, g, b] = noise(11, false);
      [
        r.iter().map(|&v| 223 + v / 8).collect(),
        g.iter().map(|&v| 223 + v / 8).collect(),
        b.iter().map(|&v| 223 + v / 8).collect(),
      ]
    }
    26 => by_pixel(&|x, y| ((x * y / 64) as u8, (x * 2) as u8, (y * 2) as u8)),
    27 => by_pixel(&|x, y| {
      // Diagonal stripes.
      if (x + y) % 24 < 12 {
        (230, 180, 40)
      } else {
        (40, 60, 120)
      }
    }),
    28 => by_pixel(&|x, y| {
      // One bright pixel on black: maximal single-coefficient energy.
      if x == 37 && y == 91 {
        (255, 255, 255)
      } else {
        (0, 0, 0)
      }
    }),
    29 => by_pixel(&|x, y| {
      // Quadrants of distinct colors.
      match (x < 64, y < 64) {
        (true, true) => (220, 30, 30),
        (false, true) => (30, 220, 30),
        (true, false) => (30, 30, 220),
        (false, false) => (220, 220, 30),
      }
    }),
    _ => panic!("test image case {n} out of range (0..{NUM_CASES})"),
  }
}
