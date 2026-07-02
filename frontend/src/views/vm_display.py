"""Framebuffer display widget (Phase 6).

Receives the framebuffer memfd (passed by the daemon over SCM_RIGHTS), mmaps it,
and renders it as a QImage on a ~30 FPS timer. Because the mapping is the *same*
physical pages the guest draws into (§3.4, zero copy), guest pixel writes appear
here with no data ever crossing the socket.
"""

from __future__ import annotations

import mmap
import os

from PyQt6.QtCore import Qt, QTimer
from PyQt6.QtGui import QImage, QPainter, QPixmap
from PyQt6.QtWidgets import QWidget


class VmDisplay(QWidget):
    def __init__(self, parent=None) -> None:
        super().__init__(parent)
        self.setMinimumSize(320, 240)
        self._mm: mmap.mmap | None = None
        self._fd: int | None = None
        self._w = 0
        self._h = 0
        self._image: QImage | None = None

        self._timer = QTimer(self)
        self._timer.setInterval(33)  # ~30 FPS
        self._timer.timeout.connect(self._refresh)

        self.setStyleSheet("background:#000;")

    def attach(self, fd: int, width: int, height: int, size: int) -> bool:
        """Map the framebuffer memfd and start rendering."""
        self.detach()
        try:
            self._mm = mmap.mmap(fd, size, mmap.MAP_SHARED, mmap.PROT_READ)
        except (OSError, ValueError):
            os.close(fd)
            return False
        self._fd = fd
        self._w = width
        self._h = height
        # XRGB8888 — 4 bytes/pixel. QImage.Format_RGB32 is 0xffRRGGBB on LE.
        self._image = QImage(
            memoryview(self._mm), width, height, width * 4, QImage.Format.Format_RGB32
        )
        self.setMinimumSize(min(width, 1280), min(height, 800))
        self._timer.start()
        self.update()
        return True

    def detach(self) -> None:
        self._timer.stop()
        self._image = None
        if self._mm is not None:
            try:
                self._mm.close()
            except (OSError, BufferError):
                pass
            self._mm = None
        if self._fd is not None:
            try:
                os.close(self._fd)
            except OSError:
                pass
            self._fd = None

    def _refresh(self) -> None:
        # The QImage shares the mmap buffer, so it always reflects current guest
        # pixels; we just repaint.
        self.update()

    def paintEvent(self, _event) -> None:  # noqa: N802
        p = QPainter(self)
        p.fillRect(self.rect(), Qt.GlobalColor.black)
        if self._image is not None:
            # Scale to fit while keeping aspect ratio.
            pix = QPixmap.fromImage(self._image)
            scaled = pix.scaled(
                self.size(),
                Qt.AspectRatioMode.KeepAspectRatio,
                Qt.TransformationMode.SmoothTransformation,
            )
            x = (self.width() - scaled.width()) // 2
            y = (self.height() - scaled.height()) // 2
            p.drawPixmap(x, y, scaled)
        else:
            p.setPen(Qt.GlobalColor.gray)
            p.drawText(
                self.rect(),
                Qt.AlignmentFlag.AlignCenter,
                "No framebuffer.\nStart a VM with a display, then click 'Attach Display'.",
            )
        p.end()

    def closeEvent(self, event) -> None:  # noqa: N802
        self.detach()
        super().closeEvent(event)
