//! Glue between the library crates. Each function here is a small,
//! focused operation a CLI command can call without re-implementing
//! boilerplate (config loading, path resolution, state persistence).

pub mod acquirer;
pub mod acquirer_factory;
pub mod config_repo;
pub mod install;
pub mod runtime;
pub mod selector;
pub mod supervisor;
pub mod tag_resolver;
