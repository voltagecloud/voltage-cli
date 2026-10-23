//! Command-line access to the Voltage API.
//!
//! `config`, `registry`, and `secret` are public so integration tests can seed saved
//! credentials and walk the operation contract; everything else is process-internal.

mod all_orgs;
mod api;
mod auth;
mod backoff;
mod cli;
pub mod config;
mod context;
mod dry_run;
mod error;
mod input;
mod output;
mod payment;
mod pick;
mod price;
mod progress;
pub mod registry;
pub mod secret;
mod startup;
mod style;
mod terminal;
mod timeline;
mod wallet_currency;
mod wallet_shortlist;

pub(crate) use error::{Error, Result};
pub use startup::run;
