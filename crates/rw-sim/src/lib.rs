// SPDX-License-Identifier: Apache-2.0

//! Producer-independent building blocks for consuming a running simulation's
//! WRF-shaped output.
//!
//! # Current scope
//!
//! This crate deliberately contains no preprocessing frontend and no coupling
//! to any particular forecast runtime. It neither launches nor configures a
//! model: it only decides when a file another process is writing has become
//! safe to read. Anything that produces `wrfout_dNN_*` files — stock WRF or a
//! GPU model runner — is an equally valid producer.
//!
//! WRF preprocessing (WPS-style source-to-`wrfinput` initialization) and
//! forecast-runtime orchestration are *not* part of this crate and are not
//! included in this repository. They are separate efforts; nothing here stubs,
//! shims, or pretends to stand in for them.

mod error;
mod watch;

pub use error::{Result, SimError};
pub use watch::StableWrfoutWatcher;
