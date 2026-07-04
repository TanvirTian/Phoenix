//! Phase 2 standalone boot test (no socket): boots a bzImage on a KVM vCPU and
//! prints guest serial output to stdout until the guest halts or you Ctrl-C.
//!
//! Usage:
//!   cargo run --bin boot-kernel -- <bzImage> [memory_mb] [cmdline...]
//!
//! Example:
//!   cargo run --bin boot-kernel -- ./bzImage 512 "console=ttyS0 reboot=k panic=1"
//!
//! Requires /dev/kvm. `println!`/`eprintln!` are fine here (CLI test binary).

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use vmm_daemon::control::protocol::VmEvent;
use vmm_daemon::vm;

fn main() {
    // Send logs to stderr so they never contend with the guest's serial output
    // on stdout (holding stdout across threads can deadlock the console).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let kernel = match args.next() {
        Some(k) => k,
        None => {
            eprintln!("usage: boot-kernel <bzImage> [memory_mb] [cmdline...]");
            std::process::exit(2);
        }
    };
    let memory_mb: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);
    let cmdline_rest: Vec<String> = args.collect();
    let cmdline = if cmdline_rest.is_empty() {
        "console=ttyS0 reboot=k panic=1 pci=off".to_string()
    } else {
        cmdline_rest.join(" ")
    };

    // Optional initrd via INITRD env var, disk via DISK env var, TAP net via
    // NET env var (name of a pre-created tap interface, e.g. NET=tap0).
    let initrd = std::env::var("INITRD").ok();
    let disk = std::env::var("DISK").ok();
    let net = std::env::var("NET").ok();
    println!(
        "[boot-kernel] kernel={kernel} mem={memory_mb}MiB cmdline={cmdline:?} initrd={initrd:?} disk={disk:?} net={net:?}"
    );

    let (tx, rx) = std::sync::mpsc::channel::<VmEvent>();
    let stop = Arc::new(AtomicBool::new(false));

    let running = match vm::boot::boot_and_run(
        &kernel,
        &cmdline,
        memory_mb,
        initrd.as_deref(),
        disk.as_deref(),
        None, // no framebuffer in the serial-only CLI tool
        net.as_deref(),
        tx,
        stop.clone(),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[boot-kernel] boot failed: {e}");
            eprintln!("[boot-kernel] note: requires /dev/kvm.");
            std::process::exit(1);
        }
    };

    // Ctrl-C stops the vCPU.
    {
        let stop = stop.clone();
        let _ = ctrlc_lite(move || stop.store(true, std::sync::atomic::Ordering::SeqCst));
    }

    // Forward host keystrokes (stdin) into the guest UART so the serial shell is
    // interactive. Reads raw bytes; press Ctrl-C to stop the VM. For a proper
    // TTY experience run your terminal in raw mode, but line-buffered stdin
    // already lets you type commands and press Enter.
    {
        let running_in = running.clone();
        std::thread::Builder::new()
            .name("stdin-forward".into())
            .spawn(move || {
                use std::io::Read;
                let mut stdin = std::io::stdin().lock();
                let mut buf = [0u8; 256];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break, // EOF
                        Ok(n) => running_in.feed_serial(&buf[..n]),
                        Err(_) => break,
                    }
                }
            })
            .ok();
    }

    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(VmEvent::SerialOutput(bytes)) => {
                // Lock stdout per-write (do NOT hold the lock across the loop —
                // that can deadlock with the tracing logger which also writes
                // to stdout from other threads).
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            Ok(VmEvent::Exited(reason)) => {
                println!("\n[boot-kernel] VM exited: {reason}");
                break;
            }
            Ok(VmEvent::StateChanged(_)) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    println!("\n[boot-kernel] stopping");
                    running.stop();
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Minimal Ctrl-C handler using libc's signal() to avoid an extra dependency.
fn ctrlc_lite<F: Fn() + Send + Sync + 'static>(f: F) -> Result<(), ()> {
    use std::sync::OnceLock;
    static HANDLER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
    // We can only install a plain fn pointer with libc::signal, so route through
    // a static. This is best-effort for the CLI test tool.
    let boxed: Box<dyn Fn() + Send + Sync> = Box::new(f);
    let _ = HANDLER.set(boxed);
    extern "C" fn on_sigint(_: libc::c_int) {
        if let Some(h) = HANDLER.get() {
            h();
        }
    }
    let handler: extern "C" fn(libc::c_int) = on_sigint;
    unsafe {
        libc::signal(libc::SIGINT, handler as usize as libc::sighandler_t);
    }
    Ok(())
}
