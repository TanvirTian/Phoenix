"""UDS client for the vmm-daemon control plane.

Wire protocol (see `crates/vmm-daemon/src/control/protocol.rs`):
    frame = u32 big-endian length prefix + JSON payload

Blocking socket I/O runs on a background QThread; decoded frames are delivered
to the GUI thread via Qt signals so the UI never blocks. This is the only channel
between the GUI and the VMM core (spec §1: UDS, no FFI). The client is resilient:
it never raises on send when disconnected, and exposes connection state so the UI
can auto-retry and report a daemon crash without dying itself.
"""

from __future__ import annotations

import json
import socket
import struct
import threading
from typing import Any, Optional

from PyQt6.QtCore import QObject, QThread, pyqtSignal


def _frame(payload: bytes) -> bytes:
    return struct.pack(">I", len(payload)) + payload


class _ReaderThread(QThread):
    """Blocking reader: pulls length-prefixed JSON frames off the socket.

    Supports a cooperative *pause* so the owner can safely take over the socket
    for an out-of-band SCM_RIGHTS fd receive. When paused, the reader parks at a
    frame boundary (never mid-frame) and does not touch the socket, so the owner
    can recvmsg without racing it.
    """

    frame_received = pyqtSignal(dict)
    disconnected = pyqtSignal(str)

    def __init__(self, sock: socket.socket) -> None:
        super().__init__()
        self._sock = sock
        self._running = True
        self._pause = threading.Event()
        self._parked = threading.Event()

    def stop(self) -> None:
        self._running = False

    def request_pause(self) -> None:
        """Ask the reader to park at the next frame boundary and wait until it
        confirms it is parked (not holding the socket)."""
        self._parked.clear()
        self._pause.set()
        # Wake it out of its short recv wait; wait for it to park.
        self._parked.wait(2.0)

    def resume(self) -> None:
        self._pause.clear()

    def _recv_exact(self, n: int) -> Optional[bytes]:
        buf = bytearray()
        while len(buf) < n:
            try:
                chunk = self._sock.recv(n - len(buf))
            except socket.timeout:
                if not self._running:
                    return None
                continue  # keep waiting for the rest of this frame
            except OSError:
                return None
            if not chunk:
                return None
            buf.extend(chunk)
        return bytes(buf)

    def run(self) -> None:
        reason = "connection closed"
        # Short timeout so we can honor pause requests promptly.
        self._sock.settimeout(0.2)
        while self._running:
            # Park here (frame boundary) while paused so the owner can use the
            # socket for the fd receive.
            if self._pause.is_set():
                self._parked.set()
                while self._pause.is_set() and self._running:
                    self.msleep(20)
                # The owner may have changed the socket timeout during its
                # inline recvmsg; restore ours before reading again.
                try:
                    self._sock.settimeout(0.2)
                except OSError:
                    pass
                continue

            # Peek for a header with a short timeout; loop if nothing yet.
            try:
                first = self._sock.recv(4, socket.MSG_PEEK)
            except socket.timeout:
                continue
            except OSError:
                break
            if not first:
                break
            if len(first) < 4:
                # Rare partial peek; fall through to a blocking exact read.
                pass

            hdr = self._recv_exact(4)
            if hdr is None:
                if self._pause.is_set():
                    continue
                break
            (length,) = struct.unpack(">I", hdr)
            if length == 0 or length > 64 * 1024 * 1024:
                reason = f"bad frame length {length}"
                break
            payload = self._recv_exact(length)
            if payload is None:
                break
            try:
                obj = json.loads(payload)
            except json.JSONDecodeError as e:
                reason = f"bad JSON: {e}"
                break
            self.frame_received.emit(obj)
        if self._running:
            self.disconnected.emit(reason)


class DaemonClient(QObject):
    """High-level client the GUI uses.

    Signals:
        connected()             — socket established
        connection_failed(str)  — a connect attempt failed
        disconnected(str)       — connection lost (GUI survives; §1)
        response(dict)          — request/response frame (id != 0)
        serial_output(bytes)    — guest serial bytes
        state_changed(str, str) — (vm_id, new_state)
        vm_exited(str, str)     — (vm_id, reason)
    """

    connected = pyqtSignal()
    connection_failed = pyqtSignal(str)
    disconnected = pyqtSignal(str)
    response = pyqtSignal(dict)
    serial_output = pyqtSignal(bytes)
    state_changed = pyqtSignal(str, str)
    vm_exited = pyqtSignal(str, str)

    def __init__(self, socket_path: str = "/tmp/vmm.sock") -> None:
        super().__init__()
        self._socket_path = socket_path
        self._sock: Optional[socket.socket] = None
        self._reader: Optional[_ReaderThread] = None
        self._send_lock = threading.Lock()
        self._next_id = 1

    # --- lifecycle ---------------------------------------------------------

    def is_connected(self) -> bool:
        return self._sock is not None

    def connect(self) -> bool:
        if self._sock is not None:
            return True
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(self._socket_path)
        except OSError as e:
            self.connection_failed.emit(str(e))
            return False
        self._sock = s
        self._reader = _ReaderThread(s)
        self._reader.frame_received.connect(self._on_frame)
        self._reader.disconnected.connect(self._on_disconnect)
        self._reader.start()
        self.connected.emit()
        return True

    def close(self) -> None:
        if self._reader is not None:
            self._reader.stop()
            self._reader = None
        self._teardown_socket()

    def _teardown_socket(self) -> None:
        if self._sock is not None:
            try:
                self._sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                self._sock.close()
            except OSError:
                pass
            self._sock = None

    # --- sending commands (never raises; returns id or None) ---------------

    def _send(self, command: Any) -> Optional[int]:
        with self._send_lock:
            if self._sock is None:
                return None
            req_id = self._next_id
            self._next_id += 1
            req = {"id": req_id, "command": command}
            try:
                self._sock.sendall(_frame(json.dumps(req).encode()))
            except OSError as e:
                self.disconnected.emit(str(e))
                self._teardown_socket()
                return None
            return req_id

    def subscribe(self) -> Optional[int]:
        return self._send("Subscribe")

    def list_vms(self) -> Optional[int]:
        return self._send("ListVms")

    def create_vm(
        self,
        name: str,
        memory_mb: int,
        vcpus: int,
        kernel: Optional[str] = None,
        cmdline: Optional[str] = None,
        disk: Optional[str] = None,
        initrd: Optional[str] = None,
        framebuffer=None,
    ) -> Optional[int]:
        return self._send(
            {
                "CreateVm": {
                    "name": name,
                    "memory_mb": memory_mb,
                    "vcpus": vcpus,
                    "kernel": kernel,
                    "cmdline": cmdline,
                    "disk": disk,
                    "initrd": initrd,
                    "framebuffer": framebuffer,
                }
            }
        )

    def start_vm(self, vm_id: str) -> Optional[int]:
        return self._send({"StartVm": {"id": vm_id}})

    def stop_vm(self, vm_id: str) -> Optional[int]:
        return self._send({"StopVm": {"id": vm_id}})

    def send_serial_input(self, vm_id: str, data: bytes) -> Optional[int]:
        return self._send({"SendSerialInput": {"id": vm_id, "data": list(data)}})

    def request_framebuffer(self, vm_id: str) -> Optional[tuple]:
        """Synchronously request the framebuffer and receive its fd.

        This is done inline (not via the async reader) because the fd arrives as
        SCM_RIGHTS ancillary data that a plain recv() would drop. We pause the
        reader thread, send the request, read the JSON response frame, then
        recvmsg the fd, then resume the reader. Returns
        (fd, width, height, size) or None.
        """
        import array

        if self._sock is None:
            return None
        self.pause_reader()
        try:
            # Send the request.
            rid = self._send({"RequestFramebuffer": {"id": vm_id}})
            if rid is None:
                return None

            self._sock.settimeout(3.0)

            # Read the length-prefixed JSON response frame (the daemon sends this
            # BEFORE the fd message).
            hdr = self._recv_all(4)
            if hdr is None:
                return None
            (length,) = struct.unpack(">I", hdr)
            payload = self._recv_all(length)
            if payload is None:
                return None
            resp = json.loads(payload)
            result = resp.get("result", {})
            if not (isinstance(result, dict) and "Ok" in result):
                # Error (e.g. no framebuffer) — surface via response signal.
                self.response.emit(resp)
                return None
            body = result["Ok"]
            if not (isinstance(body, dict) and "FramebufferIncoming" in body):
                self.response.emit(resp)
                return None
            fbi = body["FramebufferIncoming"]

            # Now recvmsg the fd (1-byte payload + SCM_RIGHTS ancillary).
            fds = array.array("i")
            _msg, ancdata, _flags, _addr = self._sock.recvmsg(
                16, socket.CMSG_LEN(fds.itemsize)
            )
            for level, ctype, cdata in ancdata:
                if level == socket.SOL_SOCKET and ctype == socket.SCM_RIGHTS:
                    n = len(cdata) - (len(cdata) % fds.itemsize)
                    fds.frombytes(cdata[:n])
                    if len(fds) > 0:
                        return (
                            int(fds[0]),
                            int(fbi["width"]),
                            int(fbi["height"]),
                            int(fbi["size"]),
                        )
            return None
        except (OSError, ValueError, json.JSONDecodeError):
            return None
        finally:
            if self._sock is not None:
                try:
                    self._sock.settimeout(None)
                except OSError:
                    pass
            self.resume_reader()

    def _recv_all(self, n: int) -> Optional[bytes]:
        assert self._sock is not None
        buf = bytearray()
        while len(buf) < n:
            chunk = self._sock.recv(n - len(buf))
            if not chunk:
                return None
            buf.extend(chunk)
        return bytes(buf)

    def pause_reader(self) -> None:
        # Park the reader at a frame boundary; it releases the socket but stays
        # alive (so we don't lose buffered stream position).
        if self._reader is not None:
            self._reader.request_pause()

    def resume_reader(self) -> None:
        if self._reader is not None:
            self._reader.resume()

    # --- incoming frames ---------------------------------------------------

    def _on_frame(self, obj: dict) -> None:
        result = obj.get("result")
        if isinstance(result, dict) and "Ok" in result:
            body = result["Ok"]
            if isinstance(body, dict) and "VmEvent" in body:
                ev = body["VmEvent"]
                self._dispatch_event(ev.get("id", ""), ev.get("event"))
                return
        self.response.emit(obj)

    def _dispatch_event(self, vm_id: str, event: Any) -> None:
        if isinstance(event, dict):
            if "SerialOutput" in event:
                self.serial_output.emit(bytes(event["SerialOutput"]))
            elif "StateChanged" in event:
                self.state_changed.emit(vm_id, event["StateChanged"])
            elif "Exited" in event:
                self.vm_exited.emit(vm_id, event["Exited"])

    def _on_disconnect(self, reason: str) -> None:
        self._teardown_socket()
        self.disconnected.emit(reason)
