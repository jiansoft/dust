//! Application entry point and module wiring for the `dust` binary.

#![warn(missing_docs)]

mod app;
mod cleanup;
mod cli;
mod interactive;
mod update;

use clap::Parser;
use cli::Cli;
use std::error::Error;

/// Parses command-line arguments and dispatches execution to the app layer.
fn main() -> Result<(), Box<dyn Error>> {
    app::run(Cli::parse())
}
