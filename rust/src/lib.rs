//! sm (simple-memory), a port of the Python package `simple_memory`.
//!
//! Comments that name a Python function or test point at the git tag `python-0.3.2`, the last
//! commit that holds the Python build.

pub mod cli;
pub mod clock;
pub mod config;
pub mod core;
pub mod error;
#[cfg(any(test, feature = "parity"))]
pub mod parity;
pub mod py;
pub mod service;
pub mod sources;
pub mod store;
