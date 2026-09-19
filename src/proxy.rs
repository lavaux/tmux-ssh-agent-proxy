use crate::ssh_agent;
use crate::state::StateFile;
use crate::tmux;
use anyhow::Result;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub struct ProxyAgent {
    state_file: Arc<StateFile>,
}

impl ProxyAgent {
    pub fn new() -> Self {
        let runtime_dir = StateFile::runtime_dir();
        let state_file = Arc::new(StateFile::new(runtime_dir.join("state")));
        Self { state_file }
    }

    pub fn run(&self, ready: Option<Box<dyn FnOnce() + Send>>) -> Result<()> {
        let runtime_dir = StateFile::runtime_dir();
        let hex: String = (0..16).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
        let socket_path = runtime_dir.join(hex);

        let listener = UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;

        tmux::set_environment("SSH_AUTH_SOCK", &socket_path.to_string_lossy())?;

        if let Some(f) = ready {
            f();
        }

        loop {
            if !tmux::ping() {
                break;
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    let state_file = Arc::clone(&self.state_file);
                    thread::spawn(move || {
                        serve_client(stream, &state_file);
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_secs(3));
                }
                Err(e) => {
                    eprintln!("accept error: {}", e);
                    break;
                }
            }
        }

        let _ = std::fs::remove_file(&socket_path);

        Ok(())
    }
}

fn serve_client(mut downstream: UnixStream, state_file: &StateFile) {
    let agent_path = match get_agent_for_client(state_file) {
        Some(p) => p,
        None => return,
    };

    let mut upstream = match UnixStream::connect(&agent_path) {
        Ok(s) => s,
        Err(_) => return,
    };

    loop {
        let payload = match ssh_agent::recv_packet(&mut downstream) {
            Ok(Some(p)) => p,
            _ => break,
        };

        let response = match ssh_agent::request(&mut upstream, &payload) {
            Ok(Some(r)) => r,
            _ => {
                let _ = ssh_agent::send_packet(&mut downstream, &[ssh_agent::SSH_AGENT_FAILURE]);
                break;
            }
        };

        if ssh_agent::send_packet(&mut downstream, &response).is_err() {
            break;
        }
    }
}

fn get_agent_for_client(state_file: &StateFile) -> Option<String> {
    state_file
        .with_lock(|state| {
            let tty = tmux::current_client()?;
            state
                .agents
                .get(&tty)
                .cloned()
                .or_else(|| state.default_agent.clone())
        })
        .ok()?
}
