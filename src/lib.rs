//! A single-user Atuin sync server for Cloudflare Workers and D1.
//!
//! Everything except `worker` is plain Rust over `http` types and builds and tests natively;
//! `worker` is the wasm-only glue that reads the bindings and talks to D1.

pub mod auth;
pub mod caps;
pub mod proto;
pub mod sql;
pub mod wire;

#[cfg(target_arch = "wasm32")]
mod worker;
