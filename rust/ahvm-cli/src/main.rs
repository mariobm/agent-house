//! Rust control client. Guest argv is passed as an array, never shell-joined.
mod client;
mod commands;
mod hosts;
mod session;

use clap::Parser;
use commands::Cli;

fn main() {
    let code = match commands::run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ahvm: {e}");
            1
        }
    };
    std::process::exit(code);
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

mod distribution;
mod images;

mod upgrade;
