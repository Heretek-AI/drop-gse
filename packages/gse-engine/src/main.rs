//! Binary entrypoint for the gse-engine CLI.

use clap::Parser;
use gse_engine::cli::{run, Cli};

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
        }
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}
