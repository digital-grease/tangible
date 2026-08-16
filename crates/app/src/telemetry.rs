// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tracing setup.
//!
//! Structured logging only: `println!` is never used for application logs.
//! The one exception in this binary is `tangible openapi`, whose stdout *is*
//! its output rather than a log.

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::CommonConfig;

/// Install the global tracing subscriber.
///
/// # Errors
///
/// Returns an error if the filter directive is malformed or a subscriber has
/// already been installed.
pub fn init(common: &CommonConfig) -> Result<()> {
    let filter = EnvFilter::try_new(&common.log)
        .with_context(|| format!("parsing the log filter {:?}", common.log))?;

    let registry = tracing_subscriber::registry().with(filter);

    // Logs go to stderr so that stdout stays a clean channel for command
    // output such as the OpenAPI document.
    if common.log_json {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(std::io::stderr),
            )
            .try_init()
            .context("installing the JSON tracing subscriber")
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .try_init()
            .context("installing the tracing subscriber")
    }
}
