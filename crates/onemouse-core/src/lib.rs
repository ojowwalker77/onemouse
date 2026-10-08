//! The portable brain of onemouse, shared by every platform's app: where the
//! other machine's screens sit (`layout`), routing input between the two
//! (`controller`), translating shortcuts between operating systems
//! (`translate`) and remembering the arrangement (`config`).
//!
//! No platform code: builds and tests everywhere.

pub mod config;
pub mod controller;
pub mod layout;
pub mod translate;
