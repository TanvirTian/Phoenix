//! UDS control server (§3.3): one tokio task per client connection.
//!
//! Wire format: big-endian u32 length prefix + JSON (see `protocol.rs`). Each
//! connection can issue request/response commands and, after `Subscribe`,
//! receive asynchronous `VmEvent`s (framed responses with id 0).

use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::control::manager::Manager;
use crate::control::protocol::{Command, Request, Response, ResponseBody};

/// Maximum accepted frame size (guards against a malicious length prefix).
const MAX_FRAME: u32 = 8 * 1024 * 1024;

/// Bind the UDS listener, removing any stale socket file first.
pub async fn serve(socket_path: &str, manager: Arc<Manager>) -> anyhow::Result<()> {
    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    info!(socket = %socket_path, "control plane listening");

    loop {
        let (stream, _addr) = listener.accept().await?;
        let mgr = manager.clone();
        // One task per client (§1.1).
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, mgr).await {
                warn!(error = %e, "client task ended with error");
            }
        });
    }
}

async fn read_frame(stream: &mut (impl AsyncReadExt + Unpin)) -> anyhow::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        anyhow::bail!("frame too large: {len} bytes");
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn handle_client(stream: UnixStream, manager: Arc<Manager>) -> anyhow::Result<()> {
    info!("client connected");
    let (mut rd, wr) = stream.into_split();
    // The writer is shared between the request/response loop and the event
    // forwarding task once the client subscribes.
    let wr = Arc::new(Mutex::new(wr));

    // Spawn an event-forwarding task: it stays idle (no subscription) until the
    // client sends Subscribe, at which point we begin draining the broadcast.
    let mut events_rx = manager.events.subscribe();
    let ev_writer = wr.clone();
    let subscribed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sub_flag = subscribed.clone();
    let event_task = tokio::spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(tagged) => {
                    if !sub_flag.load(std::sync::atomic::Ordering::Relaxed) {
                        continue;
                    }
                    let resp = Response::ok(
                        0,
                        ResponseBody::VmEvent {
                            id: tagged.vm_id,
                            event: tagged.event,
                        },
                    );
                    if let Ok(frame) = resp.to_frame() {
                        let mut w = ev_writer.lock().await;
                        if w.write_all(&frame).await.is_err() {
                            break;
                        }
                        let _ = w.flush().await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(missed = n, "event subscriber lagged");
                }
                Err(_) => break,
            }
        }
    });

    // Request/response loop.
    while let Some(payload) = read_frame(&mut rd).await? {
        let req: Request = match serde_json::from_slice(&payload) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(0, format!("bad request: {e}"));
                write_response(&wr, &resp).await?;
                continue;
            }
        };

        if matches!(req.command, Command::Subscribe) {
            subscribed.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        // Note if this is a framebuffer request so we can pass the fd afterward.
        let fb_vm_id = match &req.command {
            Command::RequestFramebuffer { id } => Some(id.clone()),
            _ => None,
        };

        let resp = match manager.handle(req.command).await {
            Ok(body) => Response::ok(req.id, body),
            Err(e) => Response::err(req.id, e.to_string()),
        };
        let ok = resp.result.is_ok();
        write_response(&wr, &resp).await?;

        // After a successful RequestFramebuffer, pass the memfd out-of-band via
        // SCM_RIGHTS (JSON can't carry an fd). §3.4.
        if ok {
            if let Some(vm_id) = fb_vm_id {
                if let Some(fd) = manager.framebuffer_fd(&vm_id).await {
                    if let Err(e) = send_fd(&wr, fd).await {
                        warn!(error = %e, "failed to send framebuffer fd");
                    }
                }
            }
        }
    }

    info!("client disconnected");
    event_task.abort();
    Ok(())
}

/// Send a single fd to the client over SCM_RIGHTS, with a 1-byte payload so the
/// GUI has something to `recvmsg`. The fd is duplicated into the peer by the
/// kernel; our original stays owned by the VM.
async fn send_fd(
    wr: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    fd: std::os::fd::RawFd,
) -> anyhow::Result<()> {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    use std::io::IoSlice;
    use std::os::fd::AsRawFd;

    let w = wr.lock().await;
    let sock_fd = w.as_ref().as_raw_fd();
    let fds = [fd];
    let cmsg = [ControlMessage::ScmRights(&fds)];
    let iov = [IoSlice::new(b"F")]; // 1-byte marker payload
    // SAFETY: sock_fd is a valid connected UDS fd owned by `w` for this scope.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(sock_fd) };
    sendmsg::<()>(borrowed.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)?;
    Ok(())
}

async fn write_response(
    wr: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    resp: &Response,
) -> anyhow::Result<()> {
    let frame = resp.to_frame()?;
    let mut w = wr.lock().await;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

// Keep `error!` referenced even if all call sites are behind cfg in future.
#[allow(dead_code)]
fn _use_error() {
    error!("unreachable");
}
