//! eris-server: the HTTP service. Library form so integration tests can
//! assemble a full server in-process; `main.rs` is a thin binary wrapper.

pub mod auth;
pub mod config;
pub mod dto;
pub mod error;
pub mod handlers;
pub mod lifecycle;
pub mod metrics;
pub mod state;
