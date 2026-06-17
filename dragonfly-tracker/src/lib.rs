//! Peer-discovery tracker for Dragonfly GGUF P2P distribution.
//!
//! Exposed as a library so the warp routes and in-memory store can be embedded
//! (e.g. started in-process by integration tests) in addition to running as the
//! `dragonfly-tracker` binary.

pub mod routes;
pub mod store;
