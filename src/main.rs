// main.rs
mod cli;
mod config;
mod process;
mod queue;
mod scheduler;
mod walker;

use crate::scheduler::Scheduler;
use clap::Parser;
use cli::Cli;
use config::Config;

fn main() {
    let args = Cli::parse();
    let config = Config::from_cli(&args); // could inline to 1 line
    Scheduler::run(&config, &args);
}
