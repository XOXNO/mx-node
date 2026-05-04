//! Library surface for the xtask crate. Modules are exposed here
//! purely so integration tests can exercise pure-logic components
//! (CSV writer, winner-selection rules, toml patcher) without going
//! through the binary entry point.

pub mod build;
pub mod csv;
pub mod matrix;
pub mod measure;
pub mod report;
pub mod tools;
pub mod toml_patch;
pub mod winners;
