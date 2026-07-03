#!/usr/bin/env python3
"""
PyQt6 GUI that talks to `vmm-daemon` over a Unix Domain Socket (no FFI, per
the master spec's Process Boundary rule). Configure a VM, Start/Stop it, and use
the interactive serial console.

Usage:
    python3 src/main.py [--socket /tmp/vmm.sock]

Prereq: run the daemon first, e.g.
    cargo run --bin vmm-daemon -- --socket /tmp/vmm.sock
"""

from __future__ import annotations

import argparse
import sys

from PyQt6.QtWidgets import QApplication

from views.main_window import MainWindow


def main() -> int:
    parser = argparse.ArgumentParser(description="Phoenix VMM frontend")
    parser.add_argument("--socket", default="/tmp/vmm.sock", help="daemon UDS path")
    args = parser.parse_args()

    app = QApplication(sys.argv)
    app.setApplicationName("Phoenix VMM")
    win = MainWindow(socket_path=args.socket)
    win.show()
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
