//! Golden-fixture generation: feed the deterministic synthetic images to the
//! C++ oracle service and record the hashes it computes. The committed output
//! (`testdata/golden_channels.json`) is what eris-core's golden tests verify
//! `from_channels` against – with no oracle or docker needed at test time.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Serialize, Deserialize)]
pub struct GoldenCase {
  pub case: u32,
  pub hash: String,
}

pub async fn generate(oracle_url: &str, out: &Path) -> Result<()> {
  let client = reqwest::Client::new();
  let mut cases = Vec::new();
  for case in 0..eris_core::testimages::NUM_CASES {
    let [r, g, b] = eris_core::testimages::case(case);
    let body = json!({ "channels": { "r": r, "g": g, "b": b } });
    let response = client
      .post(format!("{oracle_url}/images/{}", 900_000_000 + case))
      .json(&body)
      .send()
      .await
      .with_context(|| format!("oracle POST for case {case}"))?;
    anyhow::ensure!(
      response.status().is_success(),
      "oracle returned {} for case {case}",
      response.status()
    );
    let body: serde_json::Value = response.json().await?;
    let hash = body["hash"]
      .as_str()
      .with_context(|| format!("oracle response for case {case} lacks hash"))?
      .to_owned();

    // Sanity: our own transform must agree right now.
    let ours = eris_core::hash::encode(&eris_core::from_channels(
      &eris_core::testimages::case(case)[0],
      &eris_core::testimages::case(case)[1],
      &eris_core::testimages::case(case)[2],
    ));
    if ours != hash {
      eprintln!(
        "WARNING: case {case} disagrees with the oracle\n  oracle: {hash}\n  ours:   {ours}"
      );
    }
    cases.push(GoldenCase { case, hash });
  }
  std::fs::write(out, serde_json::to_string_pretty(&cases)?)?;
  println!("wrote {} golden cases to {}", cases.len(), out.display());
  Ok(())
}
