use eris_core::NUM_PIXELS_SQUARED;
use serde::Deserialize;

use crate::error::ApiError;

pub const MAX_LIMIT: usize = 200;
pub const DEFAULT_LIMIT: usize = 10;

/// Raw channel arrays as posted by the client (libvips pixel data via
/// IqdbProxy). Values are validated to be exactly 16384 integers of 0..=255
/// per channel – a 400 instead of the C++'s silent modulo-256 wrap.
#[derive(Debug, Deserialize)]
pub struct Channels {
  pub r: Vec<i64>,
  pub g: Vec<i64>,
  pub b: Vec<i64>,
}

impl Channels {
  pub fn validate(self) -> Result<[Vec<u8>; 3], ApiError> {
    fn one(name: &str, values: Vec<i64>) -> Result<Vec<u8>, ApiError> {
      if values.len() != NUM_PIXELS_SQUARED {
        return Err(ApiError::BadRequest(format!(
          "channel `{name}` must contain exactly {NUM_PIXELS_SQUARED} values, got {}",
          values.len()
        )));
      }
      values
        .into_iter()
        .map(|v| {
          u8::try_from(v).map_err(|_| {
            ApiError::BadRequest(format!(
              "channel `{name}` contains value {v} outside 0..=255"
            ))
          })
        })
        .collect()
    }
    Ok([one("r", self.r)?, one("g", self.g)?, one("b", self.b)?])
  }
}

#[derive(Debug, Deserialize)]
pub struct ImagePostBody {
  pub channels: Channels,
}

#[derive(Debug, Deserialize)]
pub struct QueryBody {
  // Precedence: hash > channels > post_id, matching the C++.
  pub hash: Option<String>,
  pub channels: Option<Channels>,
  pub post_id: Option<u32>,
  pub limit: Option<i64>,
  pub min_score: Option<f64>,
}

pub fn clamp_limit(limit: Option<i64>) -> Result<usize, ApiError> {
  match limit {
    None => Ok(DEFAULT_LIMIT),
    Some(n) if n <= 0 => Err(ApiError::BadRequest(format!(
      "limit must be positive, got {n}"
    ))),
    Some(n) => Ok((n as usize).min(MAX_LIMIT)),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn limit_rules() {
    assert_eq!(clamp_limit(None).unwrap(), 10);
    assert_eq!(clamp_limit(Some(1)).unwrap(), 1);
    assert_eq!(clamp_limit(Some(200)).unwrap(), 200);
    assert_eq!(clamp_limit(Some(100_000)).unwrap(), 200);
    assert!(clamp_limit(Some(0)).is_err());
    assert!(clamp_limit(Some(-1)).is_err());
    assert!(clamp_limit(Some(i64::MIN)).is_err());
  }

  #[test]
  fn channel_validation() {
    let ok = Channels {
      r: vec![0; NUM_PIXELS_SQUARED],
      g: vec![255; NUM_PIXELS_SQUARED],
      b: vec![128; NUM_PIXELS_SQUARED],
    };
    assert!(ok.validate().is_ok());

    let short = Channels {
      r: vec![0; 100],
      g: vec![0; NUM_PIXELS_SQUARED],
      b: vec![0; NUM_PIXELS_SQUARED],
    };
    assert!(short.validate().is_err());

    let mut r = vec![0; NUM_PIXELS_SQUARED];
    r[7] = 256;
    let out_of_range = Channels {
      r,
      g: vec![0; NUM_PIXELS_SQUARED],
      b: vec![0; NUM_PIXELS_SQUARED],
    };
    assert!(out_of_range.validate().is_err());

    let mut r = vec![0; NUM_PIXELS_SQUARED];
    r[7] = -1;
    let negative = Channels {
      r,
      g: vec![0; NUM_PIXELS_SQUARED],
      b: vec![0; NUM_PIXELS_SQUARED],
    };
    assert!(negative.validate().is_err());
  }
}
