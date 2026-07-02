"""Serial console widget: renders guest UART output and sends keystrokes back.

Includes a minimal ANSI/VT100 interpreter so the guest's color codes, backspace,
and line-erase sequences render like a real terminal instead of showing raw
escape bytes (e.g. `^[[1;34m`). It's not a full terminal emulator — just enough
for an interactive busybox shell to look right.
"""

from __future__ import annotations

from typing import Optional

from PyQt6.QtCore import Qt, pyqtSignal
from PyQt6.QtGui import (
    QColor,
    QFont,
    QKeyEvent,
    QTextCharFormat,
    QTextCursor,
)
from PyQt6.QtWidgets import QPlainTextEdit

# Standard 16 ANSI colors (normal 0-7, bright 8-15).
_ANSI_COLORS = [
    "#000000", "#cc0000", "#4e9a06", "#c4a000",
    "#3465a4", "#75507b", "#06989a", "#d3d7cf",
    "#555753", "#ef2929", "#8ae234", "#fce94f",
    "#729fcf", "#ad7fa8", "#34e2e2", "#eeeeec",
]
_DEFAULT_FG = "#d0d0d0"


class SerialConsole(QPlainTextEdit):
    """A terminal-ish view with a small ANSI interpreter."""

    input_bytes = pyqtSignal(bytes)

    def __init__(self, parent=None) -> None:
        super().__init__(parent)
        self.setReadOnly(False)
        self.setUndoRedoEnabled(False)
        self.setLineWrapMode(QPlainTextEdit.LineWrapMode.NoWrap)
        self.setMaximumBlockCount(5000)  # scrollback cap (keeps memory bounded)
        font = QFont("monospace")
        font.setStyleHint(QFont.StyleHint.Monospace)
        font.setPointSize(11)
        self.setFont(font)
        self.setStyleSheet(
            "QPlainTextEdit { background-color: #101014; color: #d0d0d0;"
            " border: 1px solid #2a2e37; border-radius: 6px; padding: 6px; }"
        )
        self._enabled_input = False

        # ANSI parser state.
        self._pending = b""          # incomplete escape sequence carried over
        self._fmt = self._base_format()

    def _base_format(self) -> QTextCharFormat:
        f = QTextCharFormat()
        f.setForeground(QColor(_DEFAULT_FG))
        return f

    def set_input_enabled(self, enabled: bool) -> None:
        self._enabled_input = enabled

    def set_font(self, font: QFont) -> None:
        """Set the console font (used by the toolbar A-/A+ controls)."""
        self.setFont(font)

    # --- output from the guest --------------------------------------------

    def append_output(self, data: bytes) -> None:
        """Feed raw guest serial bytes through the ANSI interpreter."""
        # Follow-tail: only auto-scroll if the user is already near the bottom,
        # so scrolling up to read history isn't yanked back down.
        sb = self.verticalScrollBar()
        at_bottom = sb.value() >= sb.maximum() - 4

        buf = self._pending + data
        self._pending = b""
        i = 0
        n = len(buf)
        cursor = self.textCursor()
        cursor.movePosition(QTextCursor.MoveOperation.End)

        text_run = bytearray()

        def flush_text() -> None:
            if text_run:
                cursor.insertText(
                    text_run.decode("utf-8", errors="replace"), self._fmt
                )
                text_run.clear()

        while i < n:
            b = buf[i]
            if b == 0x1B:  # ESC — start of an escape sequence
                flush_text()
                consumed = self._handle_escape(buf, i, cursor)
                if consumed is None:
                    # Incomplete sequence: stash the rest for next chunk.
                    self._pending = bytes(buf[i:])
                    break
                i = consumed
                continue
            elif b == 0x08:  # backspace — move cursor left one char
                flush_text()
                cursor.movePosition(QTextCursor.MoveOperation.Left)
                i += 1
            elif b == 0x0D:  # CR — move cursor to start of line
                flush_text()
                cursor.movePosition(QTextCursor.MoveOperation.StartOfLine)
                i += 1
            elif b == 0x0A:  # LF — newline (go to end, insert \n)
                flush_text()
                cursor.movePosition(QTextCursor.MoveOperation.End)
                cursor.insertText("\n", self._fmt)
                i += 1
            elif b == 0x07:  # BEL — ignore
                i += 1
            else:
                text_run.append(b)
                i += 1

        flush_text()
        self.setTextCursor(cursor)
        if at_bottom:
            self.ensureCursorVisible()

    def _handle_escape(self, buf: bytes, start: int, cursor: QTextCursor):
        """Handle an escape sequence beginning at buf[start] (== ESC).

        Returns the index just past the sequence, or None if the sequence is
        incomplete (needs more bytes).
        """
        n = len(buf)
        if start + 1 >= n:
            return None
        second = buf[start + 1]
        if second != ord("["):
            # Non-CSI escape (e.g. ESC( for charset). Skip ESC + one byte.
            return start + 2
        # CSI: ESC [ params... final-byte (0x40-0x7E)
        j = start + 2
        params = bytearray()
        while j < n:
            c = buf[j]
            if 0x40 <= c <= 0x7E:  # final byte
                self._apply_csi(chr(c), bytes(params), cursor)
                return j + 1
            params.append(c)
            j += 1
        return None  # incomplete

    def _apply_csi(self, final: str, params: bytes, cursor: QTextCursor) -> None:
        text = params.decode("ascii", errors="replace")
        if final == "m":
            self._apply_sgr(text)
        elif final == "K":
            # Erase in line: 0/none=to end, 1=to start, 2=whole line.
            mode = text or "0"
            if mode in ("0", ""):
                cursor.movePosition(
                    QTextCursor.MoveOperation.EndOfLine,
                    QTextCursor.MoveMode.KeepAnchor,
                )
                cursor.removeSelectedText()
            elif mode == "2":
                cursor.movePosition(QTextCursor.MoveOperation.StartOfLine)
                cursor.movePosition(
                    QTextCursor.MoveOperation.EndOfLine,
                    QTextCursor.MoveMode.KeepAnchor,
                )
                cursor.removeSelectedText()
        elif final == "J":
            # Erase in display: erase to end of view (0) — remove to end.
            cursor.movePosition(
                QTextCursor.MoveOperation.End, QTextCursor.MoveMode.KeepAnchor
            )
            cursor.removeSelectedText()
        elif final in ("C",):  # cursor forward
            steps = int(text) if text.isdigit() else 1
            for _ in range(steps):
                cursor.movePosition(QTextCursor.MoveOperation.Right)
        elif final in ("D",):  # cursor back
            steps = int(text) if text.isdigit() else 1
            for _ in range(steps):
                cursor.movePosition(QTextCursor.MoveOperation.Left)
        # Other CSI codes (cursor positioning, etc.) are ignored for a simple
        # line-oriented shell.

    def _apply_sgr(self, params: str) -> None:
        """Select Graphic Rendition — colors / bold / reset."""
        codes = [p for p in params.split(";")] if params else ["0"]
        for code in codes:
            if code in ("", "0"):
                self._fmt = self._base_format()
            elif code == "1":
                self._fmt.setFontWeight(QFont.Weight.Bold)
            elif code == "22":
                self._fmt.setFontWeight(QFont.Weight.Normal)
            elif code.isdigit():
                v = int(code)
                if 30 <= v <= 37:
                    self._fmt.setForeground(QColor(_ANSI_COLORS[v - 30]))
                elif 90 <= v <= 97:
                    self._fmt.setForeground(QColor(_ANSI_COLORS[v - 90 + 8]))
                elif v == 39:
                    self._fmt.setForeground(QColor(_DEFAULT_FG))
                elif 40 <= v <= 47:
                    self._fmt.setBackground(QColor(_ANSI_COLORS[v - 40]))
                elif v == 49:
                    self._fmt.clearBackground()

    # --- input to the guest -----------------------------------------------

    def keyPressEvent(self, event: QKeyEvent) -> None:  # noqa: N802 (Qt name)
        if not self._enabled_input:
            return
        data: Optional[bytes] = None
        key = event.key()

        if key in (Qt.Key.Key_Return, Qt.Key.Key_Enter):
            data = b"\r"
        elif key == Qt.Key.Key_Backspace:
            data = b"\x7f"
        elif key == Qt.Key.Key_Tab:
            data = b"\t"
        elif key == Qt.Key.Key_Escape:
            data = b"\x1b"
        elif key == Qt.Key.Key_Up:
            data = b"\x1b[A"
        elif key == Qt.Key.Key_Down:
            data = b"\x1b[B"
        elif key == Qt.Key.Key_Right:
            data = b"\x1b[C"
        elif key == Qt.Key.Key_Left:
            data = b"\x1b[D"
        elif (event.modifiers() & Qt.KeyboardModifier.ControlModifier) and event.text():
            ch = event.text().lower()
            if len(ch) == 1 and "a" <= ch <= "z":
                data = bytes([ord(ch) - ord("a") + 1])
        else:
            txt = event.text()
            if txt:
                data = txt.encode("utf-8")

        if data:
            self.input_bytes.emit(data)
        # No super(): the guest echoes typed chars, so we don't insert locally.
