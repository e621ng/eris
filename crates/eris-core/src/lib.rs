//! eris-core: the frozen IQDB math (Haar signatures, hash codec) and the
//! in-memory search index. No I/O lives in this crate.

pub mod haar;
pub mod hash;
pub mod index;
pub mod signature;
pub mod testimages;

pub use haar::from_channels;
pub use index::query::{query, Match};
pub use index::Index;
pub use signature::HaarSignature;

/// Side length of the fixed input image.
pub const NUM_PIXELS: usize = 128;
/// Pixels per channel (128 * 128).
pub const NUM_PIXELS_SQUARED: usize = NUM_PIXELS * NUM_PIXELS;
/// Retained Haar coefficients per color channel.
pub const NUM_COEFS: usize = 40;
