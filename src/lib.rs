//! Command-line access to the Voltage API.
//!
//! `config`, `registry`, and `secret` are public so integration tests can seed saved
//! credentials and walk the operation contract; everything else is process-internal.

mod api;
mod auth;
mod backoff;
mod cli;
pub mod config;
mod error;
mod input;
mod output;
mod payment;
mod price;
pub mod registry;
pub mod secret;
mod startup;
mod terminal;

pub(crate) use error::{Error, Result};
pub use startup::run;
