//! Wire format (§3.3): length-prefixed JSON, serde tagged enums.
//!
//! Frames are `u32` big-endian length prefix followed by that many bytes of
//! JSON. The GUI (`frontend/src/client.py`) implements the mirror of this.

use serde::{Deserialize, Serialize};

/// A request from a client (GUI) to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    CreateVm {
        name: String,
        memory_mb: u64,
        vcpus: u32,
        /// Optional path to a bzImage kernel (Phase 2+).
        #[serde(default)]
        kernel: Option<String>,
        /// Optional kernel command line.
        #[serde(default)]
        cmdline: Option<String>,
        /// Optional raw disk image for virtio-blk (Phase 4+).
        #[serde(default)]
        disk: Option<String>,
        /// Optional initrd/initramfs image.
        #[serde(default)]
        initrd: Option<String>,
        /// Optional display: framebuffer (width, height). Enables `/dev/fb0`.
        #[serde(default)]
        framebuffer: Option<(u32, u32)>,
    },
    StartVm {
        id: String,
    },
    StopVm {
        id: String,
    },
    PauseVm {
        id: String,
    },
    ResumeVm {
        id: String,
    },
    ListVms,
    /// Host keystrokes to feed into the guest serial console.
    SendSerialInput {
        id: String,
        data: Vec<u8>,
    },
    /// Ask the daemon to send the framebuffer FD over SCM_RIGHTS (Phase 6).
    RequestFramebuffer {
        id: String,
    },
    /// Subscribe this connection to async VM events (state + serial output).
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub result: Result<ResponseBody, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponseBody {
    Ok,
    Created { id: String },
    VmList { vms: Vec<VmInfo> },
    /// An asynchronous event pushed to a subscribed client (id=0 by convention).
    VmEvent { id: String, event: VmEvent },
    /// Signals that a framebuffer FD follows out-of-band via SCM_RIGHTS.
    FramebufferIncoming { width: u32, height: u32, size: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmInfo {
    pub id: String,
    pub name: String,
    pub state: String,
    pub memory_mb: u64,
    pub vcpus: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VmEvent {
    StateChanged(String),
    SerialOutput(Vec<u8>),
    /// The VM exited / crashed; carries a human-readable reason.
    Exited(String),
}

impl Request {
    pub fn to_frame(&self) -> serde_json::Result<Vec<u8>> {
        frame(&serde_json::to_vec(self)?)
    }
}

impl Response {
    pub fn ok(id: u64, body: ResponseBody) -> Self {
        Self {
            id,
            result: Ok(body),
        }
    }
    pub fn err(id: u64, msg: impl Into<String>) -> Self {
        Self {
            id,
            result: Err(msg.into()),
        }
    }
    pub fn to_frame(&self) -> serde_json::Result<Vec<u8>> {
        frame(&serde_json::to_vec(self)?)
    }
}

/// Prepend a big-endian u32 length prefix to `payload`.
pub fn frame(payload: &[u8]) -> serde_json::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_roundtrips_through_json() {
        let req = Request {
            id: 7,
            command: Command::CreateVm {
                name: "test".into(),
                memory_mb: 256,
                vcpus: 2,
                kernel: Some("bzImage".into()),
                cmdline: None,
                disk: None,
                initrd: None,
                framebuffer: None,
            },
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: Request = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.id, 7);
        assert!(matches!(back.command, Command::CreateVm { vcpus: 2, .. }));
    }

    #[test]
    fn frame_prefixes_length() {
        let f = frame(b"hello").unwrap();
        assert_eq!(&f[..4], &5u32.to_be_bytes());
        assert_eq!(&f[4..], b"hello");
    }

    #[test]
    fn response_error_serializes() {
        let r = Response::err(1, "boom");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("boom"));
    }
}
