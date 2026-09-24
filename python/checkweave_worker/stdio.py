"""Newline-delimited JSON session. One bad line does not stop the next."""

from __future__ import annotations

import json
import logging

from .constants import PROTOCOL_VERSION
from .devices import DeviceFailure
from .frames import FrameReader, FrameTooLarge
from .questions import ProtocolError, parse_evaluate, parse_shutdown


class WorkerLoop:
    def __init__(self, engine, stdin, stdout) -> None:
        self.engine = engine
        self.stdin = stdin
        self.stdout = stdout
        self.stop = False

    def request_stop(self) -> None:
        self.stop = True

    def run(self) -> int:
        reader = FrameReader(self.stdin)
        while not self.stop:
            try:
                frame = reader.read()
            except FrameTooLarge:
                self._write(
                    {
                        "version": PROTOCOL_VERSION,
                        "error": "frame exceeds 8388608 bytes",
                    }
                )
                continue
            except InterruptedError:
                if self.stop:
                    break
                continue
            if frame is None:
                break
            self._write(self.handle_frame(frame))
            if self.stop:
                break
        return 0

    def handle_frame(self, frame: bytes) -> dict:
        if frame.strip() == b"":
            return {"version": PROTOCOL_VERSION, "error": "empty frame"}
        try:
            text = frame.decode("utf-8")
        except UnicodeDecodeError:
            return {"version": PROTOCOL_VERSION, "error": "frame is not utf-8"}
        try:
            payload = json.loads(text)
        except json.JSONDecodeError:
            return {"version": PROTOCOL_VERSION, "error": "malformed json"}
        try:
            response = dispatch(self.engine, payload)
        except Exception as exc:
            logging.exception("request failed")
            response = {"version": PROTOCOL_VERSION, "error": f"{type(exc).__name__}: {exc}"}
            if isinstance(payload, dict) and isinstance(payload.get("id"), str):
                response["id"] = payload["id"]
        if response.get("shutdown") is True:
            self.stop = True
        return response

    def _write(self, payload: dict) -> None:
        encoded = json.dumps(
            payload,
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
        ).encode("utf-8")
        self.stdout.write(encoded + b"\n")
        self.stdout.flush()


def dispatch(engine, payload) -> dict:
    if not isinstance(payload, dict):
        return {"version": PROTOCOL_VERSION, "error": "request must be a JSON object"}
    request_id = payload.get("id")
    if payload.get("version") != PROTOCOL_VERSION:
        return _error("unsupported protocol version", request_id)
    operation = payload.get("op")
    if operation == "shutdown":
        try:
            identifier = parse_shutdown(payload)
        except ProtocolError as exc:
            return _error(str(exc), request_id)
        return {"version": PROTOCOL_VERSION, "id": identifier, "shutdown": True}
    if operation != "evaluate":
        return _error("unsupported op", request_id)
    try:
        request = parse_evaluate(payload)
    except ProtocolError as exc:
        return _error(str(exc), request_id)
    try:
        return engine.evaluate(request)
    except DeviceFailure as exc:
        logging.error("request %s device failure: %s", request.id, exc)
        return _error(str(exc), request.id)


def _error(message: str, request_id) -> dict:
    payload = {"version": PROTOCOL_VERSION, "error": message}
    if isinstance(request_id, str) and request_id:
        payload["id"] = request_id
    return payload
