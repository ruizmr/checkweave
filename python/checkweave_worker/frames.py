"""Bounded newline-delimited frames. Protocol bytes stay on this stream."""

from __future__ import annotations

from .constants import MAX_FRAME_BYTES


class FrameTooLarge(Exception):
    """A single line exceeded the 8MiB frame limit."""


class FrameReader:
    def __init__(self, stream, limit: int = MAX_FRAME_BYTES) -> None:
        self.stream = stream
        self.limit = limit
        self.pending = bytearray()
        self.eof = False

    def read(self) -> bytes | None:
        while True:
            newline = self.pending.find(b"\n")
            if newline != -1:
                if newline > self.limit:
                    del self.pending[: newline + 1]
                    raise FrameTooLarge()
                frame = bytes(self.pending[:newline])
                del self.pending[: newline + 1]
                return frame
            if self.eof:
                if not self.pending:
                    return None
                if len(self.pending) > self.limit:
                    self.pending.clear()
                    raise FrameTooLarge()
                frame = bytes(self.pending)
                self.pending.clear()
                return frame
            chunk = self._pull(65536)
            if not chunk:
                self.eof = True
                continue
            if not isinstance(chunk, (bytes, bytearray)):
                raise TypeError("protocol input must be binary")
            self.pending.extend(chunk)
            if len(self.pending) > self.limit and b"\n" not in self.pending:
                self._discard_line()
                raise FrameTooLarge()

    def _discard_line(self) -> None:
        newline = self.pending.find(b"\n")
        if newline != -1:
            del self.pending[: newline + 1]
            return
        self.pending.clear()
        while not self.eof:
            chunk = self._pull(65536)
            if not chunk:
                self.eof = True
                return
            newline = chunk.find(b"\n")
            if newline != -1:
                self.pending.extend(chunk[newline + 1 :])
                return

    def _pull(self, limit: int) -> bytes:
        """Read at most one available chunk.

        ``BufferedReader.read(n)`` blocks until ``n`` bytes or EOF. A live
        pipe with the writer still open never reaches either after a short
        line, so the frame is stuck. ``read1`` returns after one underlying
        read. Raw pipe ``read`` already returns the bytes that are ready.
        """
        read1 = getattr(self.stream, "read1", None)
        if callable(read1):
            chunk = read1(limit)
        else:
            chunk = self.stream.read(limit)
        if chunk is None:
            return b""
        if not isinstance(chunk, (bytes, bytearray)):
            raise TypeError("protocol input must be binary")
        return bytes(chunk)
