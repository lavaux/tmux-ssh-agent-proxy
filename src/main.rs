mod cli;
mod proxy;
mod ssh_agent;
mod state;
mod tmux;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}
