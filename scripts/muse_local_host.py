#!/usr/bin/env python3
"""Opt-in bounded link-control transport; never executes local commands."""
import json
import struct
import uuid
import time
import os
import queue
import select
import threading

MAX_PENDING = 8
MAX_IDS = 4096


class Link:
    """One control stream on an exclusively owned community Gateway."""
    def __init__(self, gateway, proto, start, emit, clock=time.monotonic):
        self.clock = clock
        self.pending, self.seen = {}, set()
        self.gateway, self.proto, self.start, self.emit = gateway, proto, start, emit
        self.stream_id = gateway.stream
        gateway.stream += 1
        self.service_id = proto.SERVICE_IDS["daemon"]
        self.register_id = str(uuid.uuid4())
        self.decoder = Decoder()
        self.http_ok = self.acked = self.registered = self.closed = False

    def send_frame(self, frame):
        self.gateway._send_envelope(self.service_id, frame.SerializeToString())

    def send(self, message):
        frame = self.proto.ServiceFrame(stream_id=self.stream_id)
        frame.body_chunk.data = encode(message)
        frame.body_chunk.end_body = False
        self.send_frame(frame)

    def open(self):
        frame = self.proto.ServiceFrame(stream_id=self.stream_id)
        frame.request.verb = "POST"
        frame.request.path = "/link-control"
        frame.request.end_body = False
        # Public SDK opens /link-control without application headers. Account
        # authentication is entirely the community Gateway's existing transport.
        self.send_frame(frame)
        self.send({"type": "req", "id": self.register_id, "method": "link.register",
                   "params": {"node_id": self.start["node_id"],
                              "display_name": self.start["display_name"], "platform": "linux",
                              "version": "0.1.0", "device_family": "homehub", "model_id": "linux",
                              "is_wakeup_supported": False, "commands_v2": self.start["commands_v2"]}})

    def fail(self, code):
        self.closed = True
        self.registered = False
        raise Failure(code)

    def frame(self, frame):
        if self.closed:
            raise Failure("stream_closed")
        if frame.stream_id != self.stream_id:
            return
        kind = frame.WhichOneof("kind")
        if kind == "response":
            if self.http_ok:
                self.fail("protocol_error")
            status = frame.response.status
            if not 200 <= status < 300:
                self.fail({401: "auth_required", 403: "link_forbidden"}.get(status, "link_rejected"))
            self.http_ok = True
            data, ended = frame.response.body, frame.response.end_body
        elif kind == "body_chunk":
            data, ended = frame.body_chunk.data, frame.body_chunk.end_body
        elif kind == "reset":
            self.fail("stream_reset")
        else:
            self.fail("protocol_error")
        if ended:
            self.fail("stream_closed")
        for message in self.decoder.feed(data):
            if (message.get("event") in ("link.unpaired", "node.unpaired")
                    or message.get("method") in ("link.unpaired", "node.unpaired")):
                self.fail("unpaired")
            if message.get("method") not in (None, "link.invoke"):
                self.fail("protocol_error")
            if message.get("id") == self.register_id and message.get("method") is None:
                if message.get("error") or message.get("ok") is False:
                    self.fail("link_rejected")
                self.acked = True
            self.announce()
            if message.get("method") == "link.invoke":
                self.invoke(message)
        self.announce()

    def announce(self):
        if self.http_ok and self.acked and not self.registered:
            self.registered = True
            self.emit({"type": "state", "state": "registered"})

    def invoke(self, message):
        self.expire()
        id, command = message.get("id"), message.get("command")
        timeout = message.get("timeout_ms", 30000)
        params = message.get("params", {})
        if (not self.registered or not text(id, 128) or id in self.seen
                or not isinstance(command, str) or command not in self.start["commands_v2"]
                or not isinstance(params, dict) or type(timeout) is not int
                or not 1 <= timeout <= 120000):
            self.fail("invalid_invoke")
        if len(self.pending) >= MAX_PENDING or len(self.seen) >= MAX_IDS:
            self.fail("budget_exceeded")
        self.seen.add(id)
        self.pending[id] = self.clock() + timeout / 1000
        self.emit({"type": "invoke", "id": id, "command": command,
                   "params": params, "timeout_ms": timeout})

    def expire(self):
        for id, deadline in list(self.pending.items()):
            if self.clock() >= deadline:
                del self.pending[id]
                self.send({"method": "link.result", "id": id, "ok": False, "error": "timeout"})

    def result(self, message):
        self.expire()
        id = message.get("id")
        if (self.closed or not text(id, 128) or id not in self.pending
                or set(message) - {"op", "id", "ok", "output", "error"}
                or message.get("op") != "result" or type(message.get("ok")) is not bool
                or "output" not in message
                or ("error" in message and not isinstance(message["error"], str))):
            raise Failure("invalid_result")
        del self.pending[id]
        result = {"method": "link.result", "id": id, "ok": message["ok"],
                  "payload": message["output"]}
        if "error" in message:
            result["error"] = message["error"]
        try:
            encode(result)
        except Failure:
            result = {"method": "link.result", "id": id, "ok": False, "error": "result_too_large"}
        self.send(result)


def text(value, limit):
    return isinstance(value, str) and 0 < len(value) <= limit and all(ord(c) >= 32 for c in value)


MAX_MESSAGE = 256 * 1024


class Failure(Exception):
    def __init__(self, code="protocol_error"):
        self.code = code
        super().__init__(code)


def pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise Failure()
        result[key] = value
    return result


def decode(raw):
    try:
        value = json.loads(raw, object_pairs_hook=pairs,
                           parse_constant=lambda _: (_ for _ in ()).throw(Failure()))
    except (ValueError, UnicodeError, RecursionError):
        raise Failure() from None
    if not isinstance(value, dict):
        raise Failure()
    return value


def encode(value):
    try:
        raw = json.dumps(value, separators=(",", ":"), allow_nan=False).encode()
    except (ValueError, TypeError, RecursionError):
        raise Failure() from None
    if len(raw) + 4 > MAX_MESSAGE:
        raise Failure("result_too_large")
    return struct.pack("<I", len(raw)) + raw


class Decoder:
    def __init__(self):
        self.buffer = bytearray()

    def feed(self, data):
        if len(self.buffer) + len(data) > MAX_MESSAGE:
            raise Failure("budget_exceeded")
        self.buffer.extend(data)
        messages = []
        while len(self.buffer) >= 4:
            size = struct.unpack_from("<I", self.buffer)[0]
            if size + 4 > MAX_MESSAGE:
                raise Failure("budget_exceeded")
            if len(self.buffer) < size + 4:
                break
            raw = bytes(self.buffer[4:4 + size])
            del self.buffer[:size + 4]
            if raw:
                if len(messages) >= 64:
                    raise Failure("budget_exceeded")
                messages.append(decode(raw))
        return messages


def validate_start(start):
    if (not isinstance(start, dict)
            or set(start) != {"op", "config_dir", "node_id", "display_name", "commands_v2"}
            or start.get("op") != "start" or not text(start.get("node_id"), 128)
            or not text(start.get("display_name"), 128)
            or not text(start.get("config_dir"), 4096)
            or not start["config_dir"].startswith("/")
            or ".." in start["config_dir"].split("/")):
        raise Failure("invalid_request")
    commands = start["commands_v2"]
    if (not isinstance(commands, dict) or len(commands) > 64
            or any(not text(key, 128) or not isinstance(value, dict)
                   for key, value in commands.items())):
        raise Failure("invalid_request")
    encode(start)


def safe_code(exc):
    if isinstance(exc, Failure):
        return exc.code
    if type(exc).__name__ == "AuthError":
        return "auth_required"
    if isinstance(exc, TimeoutError) or type(exc).__name__ == "WebSocketTimeout":
        return "transport_timeout"
    if isinstance(exc, EOFError):
        return "stream_closed"
    return "transport_error"


class Input:
    def __init__(self, fd):
        self.fd, self.buffer = fd, bytearray()

    def poll(self):
        if b"\n" not in self.buffer:
            if not select.select([self.fd], [], [], 0)[0]:
                return None
            data = os.read(self.fd, 8192)
            if not data:
                raise Failure("eof")
            self.buffer.extend(data)
        if len(self.buffer) > MAX_MESSAGE:
            raise Failure("budget_exceeded")
        if b"\n" not in self.buffer:
            return None
        raw, _, tail = self.buffer.partition(b"\n")
        self.buffer = bytearray(tail)
        return decode(raw)


def run(start, input_fd, emit, stop, connector=None, input_reader=None):
    """Main thread never waits on network IO; all queues are fail-closed bounded."""
    if connector is None:
        connector = community_connector
    incoming, outgoing = queue.Queue(16), queue.Queue(16)
    gateway_ref = []
    failure = []
    link = None
    code = "stopped"
    reader = input_reader or Input(input_fd)

    def put(item):
        try:
            incoming.put_nowait(item)
        except queue.Full:
            if not failure:
                failure.append("budget_exceeded")
            stop.set()

    class Proxy:
        def __init__(self, gateway):
            self.stream = gateway.stream
        def _send_envelope(self, service, raw):
            try:
                outgoing.put_nowait((service, raw))
            except queue.Full:
                raise Failure("budget_exceeded") from None

    def network():
        try:
            if connector is None:
                raise Failure("bridge_unavailable")
            with connector(start) as (gateway, proto):
                gateway_ref.append(gateway)
                if stop.is_set():
                    return
                put(("ready", (Proxy(gateway), proto)))
                def receive():
                    try:
                        while not stop.is_set():
                            frame = gateway._read_frame()
                            if frame.ByteSize() > MAX_MESSAGE + 8192:
                                raise Failure("budget_exceeded")
                            put(("frame", frame))
                    except Exception as exc:
                        put(("error", safe_code(exc)))
                threading.Thread(target=receive, daemon=True).start()
                while not stop.is_set():
                    try:
                        service, raw = outgoing.get(timeout=0.02)
                    except queue.Empty:
                        continue
                    # Dequeue can block across cancellation. Do not enter a new
                    # dependency send with queued private data after stop. Sends
                    # already entered before stop are not recallable.
                    if stop.is_set():
                        break
                    gateway._send_envelope(service, raw)
        except Exception as exc:
            put(("error", safe_code(exc)))

    worker = threading.Thread(target=network, daemon=True)
    try:
        validate_start(start)
        emit({"type": "state", "state": "connecting"})
        worker.start()
        deadline = time.monotonic() + 30
        while not stop.is_set():
            message = reader.poll()
            if message is not None:
                if message == {"op": "stop"}:
                    break
                if message.get("op") != "result" or link is None:
                    raise Failure("invalid_request")
                link.result(message)
            # Bounded per-tick work preserves cancellation under floods.
            for _ in range(8):
                try:
                    kind, value = incoming.get_nowait()
                except queue.Empty:
                    break
                if kind == "ready":
                    gateway, proto = value
                    link = Link(gateway, proto, start, emit)
                    link.open()
                elif kind == "frame" and link is not None:
                    link.frame(value)
                elif kind == "error":
                    raise Failure(value)
            if link is not None:
                link.expire()
            if (link is None or not link.registered) and time.monotonic() >= deadline:
                raise Failure("registration_timeout")
            stop.wait(0.01)
        if failure:
            code = failure[0]
    except Exception as exc:
        code = safe_code(exc)
    finally:
        stop.set()
        if link is not None:
            link.closed = True
            link.pending.clear()
        # Even a dependency with a blocking close cannot hold the control loop.
        if gateway_ref:
            def close():
                try:
                    gateway_ref[0].close()
                except Exception:
                    pass
            closer = threading.Thread(target=close, daemon=True)
            closer.start()
            closer.join(0.2)
        if worker.ident is not None:
            worker.join(0.2)
        state = "auth_required" if code == "auth_required" else (
            "closed" if code in ("stopped", "eof", "stream_closed", "unpaired") else "error")
        emit({"type": "state", "state": state, "code": code})


def bridge_module():
    import importlib.util
    from pathlib import Path
    spec = importlib.util.spec_from_file_location("_muse_account_bridge_local_host",
        Path(__file__).resolve().with_name("muse_account_bridge.py"))
    if spec is None or spec.loader is None:
        raise Failure("bridge_unavailable")
    bridge = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(bridge)
    return bridge


def community_connector(start):
    from contextlib import contextmanager
    @contextmanager
    def connect():
        bridge = bridge_module()
        try:
            with bridge.secure_config(start["config_dir"]) as config:
                config = {**config, "node_id": start["node_id"]}
                client = bridge.community_client(config)
                import muse
                with client._gateway(config) as gateway:
                    install_bounded_receiver(gateway)
                    yield gateway, muse
        except bridge.BridgeFailure as exc:
            raise Failure(exc.code) from None
    return connect()


MAX_NOISE = MAX_MESSAGE + 64 * 1024


def bounded_recv(websocket):
    """Bound curl websocket assembly before the external Noise reader allocates.

    Installed curl_cffi recv waits on AGAIN indefinitely, not a 20s idle timer.
    Keep that behavior: ordinary quiet links are not authentication failures.
    """
    from curl_cffi import CurlError, CurlECode, CurlInfo, CurlWsFlag
    data = bytearray()
    while not websocket.closed:
        try:
            chunk, frame = websocket.recv_fragment()
        except CurlError as exc:
            if exc.code != CurlECode.AGAIN:
                raise
            fd = websocket.curl.getinfo(CurlInfo.ACTIVESOCKET)
            if fd < 0:
                raise EOFError() from None
            select.select([fd], [], [], 0.05)
            continue
        flags = frame.flags
        if flags & CurlWsFlag.CLOSE:
            raise EOFError()
        if flags & (CurlWsFlag.PING | CurlWsFlag.PONG):
            continue
        if flags & CurlWsFlag.TEXT:
            raise Failure("protocol_error")
        if len(data) + len(chunk) + frame.bytesleft > MAX_NOISE:
            raise Failure("budget_exceeded")
        data.extend(chunk)
        if frame.bytesleft == 0 and not flags & CurlWsFlag.CONT:
            return bytes(data), flags
    raise EOFError()


def install_bounded_receiver(gateway):
    if not callable(getattr(gateway.ws, "recv_fragment", None)):
        raise Failure("bridge_unavailable")
    gateway.ws.recv = lambda: bounded_recv(gateway.ws)
    original_close = gateway.close
    closing = threading.Lock()
    def close_once():
        # The context manager and cancellation closer can race. Never run two
        # native libcurl closes concurrently, and never wait for a stuck close.
        if closing.acquire(blocking=False):
            original_close()
    gateway.close = close_once


def main():
    import signal
    import sys
    # Dependency prints (including native libraries) never enter protocol/logs.
    response_fd = os.dup(sys.stdout.fileno())
    with open(os.devnull, "w") as quiet:
        os.dup2(quiet.fileno(), sys.stdout.fileno())
        os.dup2(quiet.fileno(), sys.stderr.fileno())
    stop = threading.Event()
    def cancel(signum, frame):
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        stop.set()
    signal.signal(signal.SIGTERM, cancel)
    signal.signal(signal.SIGINT, cancel)
    os.set_blocking(response_fd, False)
    def emit(message):
        raw = encode(message)[4:] + b"\n"
        offset, deadline = 0, time.monotonic() + 0.5
        while offset < len(raw):
            if time.monotonic() >= deadline:
                raise Failure("output_blocked")
            if select.select([], [response_fd], [], 0.02)[1]:
                try:
                    offset += os.write(response_fd, raw[offset:])
                except BlockingIOError:
                    pass
    reader = Input(sys.stdin.fileno())
    try:
        deadline = time.monotonic() + 30
        start = None
        while not stop.is_set() and start is None:
            start = reader.poll()
            if time.monotonic() >= deadline:
                raise Failure("registration_timeout")
            stop.wait(0.01)
        if stop.is_set():
            emit({"type": "state", "state": "closed", "code": "stopped"})
        else:
            run(start, sys.stdin.fileno(), emit, stop, input_reader=reader)
    except Exception as exc:
        try:
            code = safe_code(exc)
            emit({"type": "state", "state": "closed" if code == "eof" else "error", "code": code})
        except Exception:
            pass
    finally:
        stop.set()
        os.close(response_fd)


if __name__ == "__main__":
    main()
