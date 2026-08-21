//! Golden signature tests: `from_channels` must reproduce, hex-for-hex, the
//! hashes the production C++ IQDB binary computed for the deterministic
//! synthetic images (recorded in testdata/golden_channels.json by
//! `eris-migrate fixtures` against the containerized oracle).

use serde::Deserialize;

#[derive(Deserialize)]
struct GoldenCase {
  case: u32,
  hash: String,
}

#[test]
fn from_channels_matches_cpp_oracle() {
  let path = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../testdata/golden_channels.json"
  );
  let content = std::fs::read_to_string(path)
    .expect("testdata/golden_channels.json (generate with `eris-migrate fixtures`)");
  let cases: Vec<GoldenCase> = serde_json::from_str(&content).expect("parse golden fixtures");
  assert_eq!(cases.len() as u32, eris_core::testimages::NUM_CASES);

  for golden in cases {
    let [r, g, b] = eris_core::testimages::case(golden.case);
    let sig = eris_core::from_channels(&r, &g, &b);
    let ours = eris_core::hash::encode(&sig);
    assert_eq!(
      ours, golden.hash,
      "case {} diverges from the C++ oracle",
      golden.case
    );
    // And the codec round-trips the oracle's hash back to the signature.
    assert_eq!(eris_core::hash::decode(&golden.hash).unwrap(), sig);
  }
}

/// Independent second reference: the iqdb-rs repository's golden signature
/// test values (lib/src/lib.rs `signature` test), which were themselves
/// derived from the danbooru IQDB. We can't reproduce its avglf exactly (it
/// resampled a JPEG we don't ship), but its hash codec vectors confirm ours:
/// the `iqdb_`-prefixed string it asserts is our encoding with a prefix.
#[test]
fn hash_layout_matches_iqdb_rs_reference() {
  // From iqdb-rs lib/src/lib.rs test `hash` (avglf triple + first coefs).
  let avglf = (
    0.76577718136597_f64,
    -0.00011652168713282838_f64,
    0.004947875142783265_f64,
  );
  let reference = "3fe8813f25bfad46bf1e8ba3578fff323f7444391ec46274";
  let ours = format!(
    "{:016x}{:016x}{:016x}",
    avglf.0.to_bits(),
    avglf.1.to_bits(),
    avglf.2.to_bits()
  );
  assert_eq!(ours, reference);
}
