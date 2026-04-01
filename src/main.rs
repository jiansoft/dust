//! Application entry point and module wiring for the `dust` binary.

mod app;
mod cleanup;
mod cli;
mod interactive;

use clap::Parser;
use cli::Cli;
use std::error::Error;

/// Parses command-line arguments and dispatches execution to the app layer.
fn main() -> Result<(), Box<dyn Error>> {
    app::run(Cli::parse())
}
