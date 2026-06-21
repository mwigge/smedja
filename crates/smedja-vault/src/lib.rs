//! `smedja-vault` — vector KV cold store for the smedja memory architecture.
//!
//! Embeddings are persisted in `SQLite` as little-endian `f32` BLOBs. Queries
//! perform a full scan and return the top-K entries by cosine similarity.
//!
//! This is the "cold" stratum: turns older than the working window are stored
//! here and retrieved on demand by semantic similarity search.

pub mod error;
pub mod similarity;
pub mod vault;

pub use error::VaultError;
pub use vault::{DiaryEntry, EmbedderIdentity, QueryResult, Vault, VaultEntry};
