mod cli;
mod proxy;
mod ssh_agent;
mod state;
mod tmux;

/// Entry point. Dispatches to CLI parser; prints errors to stderr with exit code 1.
fn main() {
    if let Err(e) = cli::run() {
        eprintln!("{:#}", e);
        std::process::exit(1);
    }
}
