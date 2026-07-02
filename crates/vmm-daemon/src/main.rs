//! `vmm-daemon` entrypoint (§2). Owns `/dev/kvm`, serves the control plane.
//!
//! Boundary rules (§1.1): `anyhow` is used here at the binary boundary; typed
//! `thiserror` errors live in the library crates. `tracing` is the logger.

use anyhow::Context;
use tracing::info;
use tracing_subscriber::EnvFilter;

use vmm_daemon::control::{self, Manager};

/// Default control socket path.
const DEFAULT_SOCKET: &str = "/tmp/vmm.sock";

fn parse_args() -> String {
    // Minimal arg parsing: `--socket PATH` (§2 main.rs responsibility).
    let mut socket = DEFAULT_SOCKET.to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" | "-s" => {
                if let Some(v) = args.next() {
                    socket = v;
                }
            }
            "--help" | "-h" => {
                println!("vmm-daemon [--socket PATH]");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    socket
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let socket = parse_args();
    info!(version = env!("CARGO_PKG_VERSION"), "starting vmm-daemon");

    let manager = Manager::new();

    // Graceful shutdown on Ctrl-C.
    let serve = control::server::serve(&socket, manager.clone());
    tokio::select! {
        res = serve => {
            res.context("control server failed")?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl-C, shutting down");
        }
    }

    // Best-effort cleanup of the socket file.
    let _ = std::fs::remove_file(&socket);
    let _ = &manager;
    Ok(())
}
