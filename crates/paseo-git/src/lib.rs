//! Checkout/git domain logic for the Paseo daemon, ported from the TS
//! `checkout-*` modules. Everything shells out to `git` with the same
//! arguments and parsing as the TS implementation and serializes to the wire
//! JSON shapes of the `checkout_*` protocol messages.

pub mod branches;
pub mod commits;
pub mod commits_list;
pub mod diff;
pub mod diff_cache;
pub mod diff_manager;
pub mod diff_parse;
pub mod diff_subscriptions;
pub mod error;
pub mod file_contents;
pub mod graph;
pub mod log_actions;
pub mod metadata_generator;
pub mod metadata_rules;
pub mod mutations;
pub mod paths;
pub mod repo;
pub mod run;
pub mod scheduler;
pub mod shortstat;
pub mod status;
pub mod worktree_commands;
pub mod worktree_meta;

pub use error::{to_checkout_error, CheckoutErrorCode, CheckoutErrorPayload, GitError, Result};
