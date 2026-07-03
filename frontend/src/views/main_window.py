"""Main window: connect to the daemon, configure & launch a VM, show its console.

Polished Phase 5 UI:
  * dark theme + a real monospace font (silences the Noto Sans warning),
  * a toolbar (Start / Stop / Reconnect / Clear / font size),
  * a collapsible config panel so the console gets the space,
  * connection resilience: auto-retry connect, graceful daemon-down handling,
    and a live connection indicator — the GUI never crashes if the daemon dies.
"""

from __future__ import annotations

import os

from PyQt6.QtCore import Qt, QTimer
from PyQt6.QtGui import QAction, QFont, QFontDatabase
from PyQt6.QtWidgets import (
    QComboBox,
    QFileDialog,
    QFormLayout,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QMainWindow,
    QSpinBox,
    QStatusBar,
    QTabWidget,
    QToolBar,
    QVBoxLayout,
    QWidget,
)

from client import DaemonClient
from views.serial_console import SerialConsole
from views.vm_display import VmDisplay

_DARK_QSS = """
QMainWindow, QWidget { background-color: #16181d; color: #d7dae0; }
QGroupBox {
    border: 1px solid #2a2e37; border-radius: 6px; margin-top: 8px; padding-top: 8px;
}
QGroupBox::title { subcontrol-origin: margin; left: 10px; padding: 0 4px; color: #8a919e; }
QLineEdit, QSpinBox, QComboBox {
    background-color: #1e2128; border: 1px solid #2a2e37; border-radius: 4px;
    padding: 4px 6px; color: #e6e9ef; selection-background-color: #3465a4;
}
QLineEdit:focus, QSpinBox:focus, QComboBox:focus { border: 1px solid #3465a4; }
QToolBar { background: #1b1e24; border: none; spacing: 6px; padding: 4px; }
QToolButton {
    background: #262a33; border: 1px solid #2f343d; border-radius: 5px;
    padding: 5px 12px; color: #e6e9ef;
}
QToolButton:hover { background: #2f343d; }
QToolButton:disabled { color: #565b66; background: #1e2128; }
QStatusBar { background: #12141a; color: #9aa1ad; }
QLabel#conn_ok { color: #8ae234; font-weight: bold; }
QLabel#conn_bad { color: #ef2929; font-weight: bold; }
QLabel#state { color: #729fcf; font-weight: bold; }
"""


def _monospace_font(size: int = 11) -> QFont:
    # Prefer a real fixed-pitch font; fall back to the platform monospace so we
    # never hit the "OpenType support missing for Noto Sans" warning.
    f = QFontDatabase.systemFont(QFontDatabase.SystemFont.FixedFont)
    for family in ("DejaVu Sans Mono", "Liberation Mono", "Monospace", "Courier New"):
        if family.lower() in (x.lower() for x in QFontDatabase.families()):
            f = QFont(family)
            break
    f.setStyleHint(QFont.StyleHint.Monospace)
    f.setFixedPitch(True)
    f.setPointSize(size)
    return f


class MainWindow(QMainWindow):
    def __init__(self, socket_path: str = "/tmp/vmm.sock") -> None:
        super().__init__()
        self.setWindowTitle("Phoenix VMM — Console")
        self.resize(960, 680)
        self.setStyleSheet(_DARK_QSS)

        self.socket_path = socket_path
        self.client = DaemonClient(socket_path)
        self.vm_id: str | None = None
        self._pending_start = False
        self._font_size = 11

        self._build_ui()
        self._wire_client()

        # Auto-reconnect: poll every 1.5s while disconnected.
        self._reconnect_timer = QTimer(self)
        self._reconnect_timer.setInterval(1500)
        self._reconnect_timer.timeout.connect(self._try_connect)
        self._reconnect_timer.start()
        self._try_connect()

    # --- UI ----------------------------------------------------------------

    def _build_ui(self) -> None:
        # Toolbar
        tb = QToolBar("Main")
        tb.setMovable(False)
        self.addToolBar(tb)

        self.act_start = QAction("▶  Start", self)
        self.act_start.triggered.connect(self._on_start)
        tb.addAction(self.act_start)

        self.act_stop = QAction("■  Stop", self)
        self.act_stop.setEnabled(False)
        self.act_stop.triggered.connect(self._on_stop)
        tb.addAction(self.act_stop)

        tb.addSeparator()

        self.act_reconnect = QAction("⟳  Reconnect", self)
        self.act_reconnect.triggered.connect(self._try_connect)
        tb.addAction(self.act_reconnect)

        self.act_clear = QAction("🗑  Clear", self)
        self.act_clear.triggered.connect(lambda: self.console.clear())
        tb.addAction(self.act_clear)

        self.act_display = QAction("🖥  Attach Display", self)
        self.act_display.setEnabled(False)
        self.act_display.triggered.connect(self._on_attach_display)
        tb.addAction(self.act_display)

        tb.addSeparator()
        act_font_dec = QAction("A-", self)
        act_font_dec.triggered.connect(lambda: self._bump_font(-1))
        tb.addAction(act_font_dec)
        act_font_inc = QAction("A+", self)
        act_font_inc.triggered.connect(lambda: self._bump_font(+1))
        tb.addAction(act_font_inc)

        # Central layout
        central = QWidget()
        root = QVBoxLayout(central)
        root.setContentsMargins(8, 8, 8, 8)
        root.setSpacing(8)

        # Config panel (collapsible via checkable group box)
        self.cfg_box = QGroupBox("VM Configuration")
        self.cfg_box.setCheckable(True)
        self.cfg_box.setChecked(True)
        form = QFormLayout(self.cfg_box)
        form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)

        self.kernel_edit = QLineEdit(os.path.expanduser("~/tiny-bzImage"))
        form.addRow("Kernel:", self._with_browse(self.kernel_edit))

        self.disk_edit = QLineEdit(os.path.expanduser("~/disk.img"))
        form.addRow("Disk:", self._with_browse(self.disk_edit))

        self.initrd_edit = QLineEdit(os.path.expanduser("~/initramfs.cpio.gz"))
        form.addRow("Initrd:", self._with_browse(self.initrd_edit))

        res_row = QHBoxLayout()
        self.mem_spin = QSpinBox()
        self.mem_spin.setRange(64, 8192)
        self.mem_spin.setValue(512)
        self.mem_spin.setSuffix(" MiB")
        res_row.addWidget(QLabel("Memory:"))
        res_row.addWidget(self.mem_spin)
        res_row.addSpacing(16)
        res_row.addWidget(QLabel("vCPUs:"))
        self.vcpu_combo = QComboBox()
        self.vcpu_combo.addItems(["1", "2", "4"])
        res_row.addWidget(self.vcpu_combo)
        res_row.addSpacing(16)
        from PyQt6.QtWidgets import QCheckBox

        self.fb_check = QCheckBox("Display (1024×768)")
        res_row.addWidget(self.fb_check)
        res_row.addStretch(1)
        res_wrap = QWidget()
        res_wrap.setLayout(res_row)
        form.addRow("Resources:", res_wrap)

        self.cmdline_edit = QLineEdit(
            "console=ttyS0 virtio_mmio.device=0x1000@0xfe000000:5"
        )
        form.addRow("Cmdline:", self.cmdline_edit)

        # Collapse behavior: hide the form contents when unchecked.
        self.cfg_box.toggled.connect(self._toggle_config)
        root.addWidget(self.cfg_box)

        # Console + Display tabs
        self.tabs = QTabWidget()
        self.console = SerialConsole()
        self.console.set_font(_monospace_font(self._font_size))
        self.console.input_bytes.connect(self._on_console_input)
        self.tabs.addTab(self.console, "Serial Console")

        self.display = VmDisplay()
        self.tabs.addTab(self.display, "Display")
        root.addWidget(self.tabs, 1)

        self.setCentralWidget(central)

        # Status bar with a connection indicator.
        sb = QStatusBar()
        self.conn_label = QLabel("● offline")
        self.conn_label.setObjectName("conn_bad")
        self.state_label = QLabel("state: —")
        self.state_label.setObjectName("state")
        sb.addPermanentWidget(self.state_label)
        sb.addPermanentWidget(self.conn_label)
        self.setStatusBar(sb)
        self._set_status("starting…")

    def _with_browse(self, edit: QLineEdit) -> QWidget:
        row = QHBoxLayout()
        row.setContentsMargins(0, 0, 0, 0)
        row.addWidget(edit, 1)
        btn = QLabel("📁")
        btn.setCursor(Qt.CursorShape.PointingHandCursor)
        btn.setToolTip("Browse…")
        btn.mousePressEvent = lambda _e, e=edit: self._browse(e)  # type: ignore
        row.addWidget(btn)
        w = QWidget()
        w.setLayout(row)
        return w

    def _toggle_config(self, on: bool) -> None:
        for child in self.cfg_box.findChildren(QWidget):
            child.setVisible(on)
        self.cfg_box.setMaximumHeight(16777215 if on else 28)

    def _browse(self, edit: QLineEdit) -> None:
        path, _ = QFileDialog.getOpenFileName(self, "Choose file")
        if path:
            edit.setText(path)

    def _bump_font(self, delta: int) -> None:
        self._font_size = max(7, min(28, self._font_size + delta))
        self.console.set_font(_monospace_font(self._font_size))

    def _set_status(self, msg: str) -> None:
        self.statusBar().showMessage(msg)

    # --- connection --------------------------------------------------------

    def _try_connect(self) -> None:
        if self.client.is_connected():
            return
        if self.client.connect():
            pass  # connected() signal handles the rest

    def _wire_client(self) -> None:
        self.client.connected.connect(self._on_connected)
        self.client.connection_failed.connect(self._on_connect_failed)
        self.client.disconnected.connect(self._on_disconnected)
        self.client.response.connect(self._on_response)
        self.client.serial_output.connect(self.console.append_output)
        self.client.state_changed.connect(self._on_state_changed)
        self.client.vm_exited.connect(self._on_vm_exited)

    def _on_connected(self) -> None:
        self.conn_label.setText("● connected")
        self.conn_label.setObjectName("conn_ok")
        self.conn_label.setStyleSheet("color:#8ae234; font-weight:bold;")
        self._set_status("connected — subscribing to events")
        self.client.subscribe()
        self.act_start.setEnabled(True)

    def _on_connect_failed(self, _err: str) -> None:
        self.conn_label.setText("● offline")
        self.conn_label.setStyleSheet("color:#ef2929; font-weight:bold;")
        self._set_status(
            "daemon not reachable — start it: "
            "cargo run --bin vmm-daemon -- --socket " + self.socket_path
        )
        self.act_start.setEnabled(False)

    def _on_disconnected(self, reason: str) -> None:
        self.conn_label.setText("● offline")
        self.conn_label.setStyleSheet("color:#ef2929; font-weight:bold;")
        self._set_status(f"DISCONNECTED: {reason} (will retry)")
        self.state_label.setText("state: —")
        self.console.set_input_enabled(False)
        self.act_start.setEnabled(False)
        self.act_stop.setEnabled(False)
        self.vm_id = None
        # auto-reconnect timer keeps running

    # --- actions -----------------------------------------------------------

    def _on_start(self) -> None:
        if not self.client.is_connected():
            self._set_status("not connected — is the daemon running?")
            return
        self.console.clear()
        self.console.append_output(b"[gui] creating VM...\r\n")
        kernel = self.kernel_edit.text().strip() or None
        disk = self.disk_edit.text().strip() or None
        initrd = self.initrd_edit.text().strip() or None
        cmdline = self.cmdline_edit.text().strip() or None
        if disk and not os.path.exists(disk):
            self.console.append_output(f"[gui] warning: disk not found: {disk}\r\n".encode())
            disk = None
        if initrd and not os.path.exists(initrd):
            self.console.append_output(
                f"[gui] warning: initrd not found: {initrd}\r\n".encode()
            )
            initrd = None

        fb = (1024, 768) if self.fb_check.isChecked() else None
        self._has_fb = fb is not None
        self._pending_start = True
        rid = self.client.create_vm(
            name="gui-vm",
            memory_mb=self.mem_spin.value(),
            vcpus=int(self.vcpu_combo.currentText()),
            kernel=kernel,
            cmdline=cmdline,
            disk=disk,
            initrd=initrd,
            framebuffer=fb,
        )
        if rid is None:
            self._set_status("send failed — daemon disconnected")
            return
        self.act_start.setEnabled(False)
        # Collapse the config panel to give the console room while running.
        self.cfg_box.setChecked(False)

    def _on_attach_display(self) -> None:
        if not self.vm_id:
            return
        self._set_status("requesting framebuffer…")
        # Synchronous: sends the request and receives the fd inline (the fd
        # can't come through the async reader — it's SCM_RIGHTS ancillary data).
        result = self.client.request_framebuffer(self.vm_id)
        if result is None:
            self._set_status("failed to attach display (no fd received)")
            return
        fd, w, h, size = result
        if self.display.attach(fd, w, h, size):
            self.tabs.setCurrentWidget(self.display)
            self._set_status(f"display attached {w}x{h}")
        else:
            self._set_status("failed to map framebuffer")

    def _on_stop(self) -> None:
        if self.vm_id:
            self.display.detach()
            self.client.stop_vm(self.vm_id)

    def _on_console_input(self, data: bytes) -> None:
        if self.vm_id:
            self.client.send_serial_input(self.vm_id, data)

    # --- responses / events ------------------------------------------------

    def _on_response(self, obj: dict) -> None:
        result = obj.get("result", {})
        if isinstance(result, dict) and "Err" in result:
            self._set_status(f"error: {result['Err']}")
            self.act_start.setEnabled(True)
            return
        body = result.get("Ok") if isinstance(result, dict) else None
        if isinstance(body, dict) and "Created" in body:
            self.vm_id = body["Created"]["id"]
            self._set_status(f"created {self.vm_id}")
            if self._pending_start:
                self._pending_start = False
                self.client.start_vm(self.vm_id)

    def _on_state_changed(self, vm_id: str, state: str) -> None:
        self.state_label.setText(f"state: {state}")
        self._set_status(f"{vm_id}: {state}")
        running = state == "Running"
        self.console.set_input_enabled(running)
        self.act_stop.setEnabled(running)
        self.act_start.setEnabled(state in ("Stopped",))
        self.act_display.setEnabled(running and getattr(self, "_has_fb", False))
        if running:
            self.console.setFocus()

    def _on_vm_exited(self, vm_id: str, reason: str) -> None:
        self.console.append_output(f"\r\n[gui] VM exited: {reason}\r\n".encode())
        self.state_label.setText("state: Stopped")
        self.console.set_input_enabled(False)
        self.act_start.setEnabled(True)
        self.act_stop.setEnabled(False)
        self.vm_id = None

    def closeEvent(self, event) -> None:  # noqa: N802
        self._reconnect_timer.stop()
        self.client.close()
        super().closeEvent(event)
