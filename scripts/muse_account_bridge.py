#!/usr/bin/env python3
"""Typed, credential-contained adapter for an external community muse-mcp install.

One stdin request/one stdout response. Never start an MCP process or expose a raw
route. Imports of the community library are deferred until explicit execution.
"""


import json
import fcntl
import os
import re
from contextlib import contextmanager
from pathlib import Path
import secrets
import stat


MAX_REQUEST = 1024 * 1024
MAX_RESPONSE = 4 * 1024 * 1024


ERROR_CODES = frozenset({"auth_required", "vm_unavailable", "timeout", "invalid_request",
    "insecure_config", "bridge_unavailable", "bridge_failure", "protocol_error", "response_too_large"})


class BridgeFailure(Exception):
    def __init__(self, code, uncertain=False):
        self.code = code if code in ERROR_CODES else "bridge_failure"
        self.uncertain = bool(uncertain)
        super().__init__(self.code)


def checked(value, expected):
    if isinstance(value, dict) and value.get("ok") is False:
        code = str(value.get("error", {}).get("code", "")).lower()
        code = {"invalid_argument": "invalid_request", "invalid_thread": "invalid_request",
                "gateway_error": "bridge_failure"}.get(code, code)
        raise BridgeFailure(code)
    if not isinstance(value, expected):
        raise BridgeFailure("protocol_error")
    return value


def classify_exception(exc, uncertain=False):
    if isinstance(exc, BridgeFailure):
        exc.uncertain = exc.uncertain or uncertain
        return exc
    if isinstance(exc, TimeoutError):
        return BridgeFailure("timeout", uncertain)
    if type(exc).__name__ == "AuthError":
        return BridgeFailure("auth_required", uncertain)
    if type(exc).__name__ == "VMUnavailableError":
        return BridgeFailure("vm_unavailable", uncertain)
    return BridgeFailure("bridge_failure", uncertain)


def optional_text(value):
    return str(value) if isinstance(value, (str, int, float)) and not isinstance(value, bool) else None


def required_text(value):
    return optional_text(value) or ""


def content_text(value):
    """Defense in depth for content from the two direct read-only task routes."""
    text = optional_text(value)
    if text is None:
        return None
    text = re.sub(r'(?i)(["\']?(?:set-cookie|cookie)["\']?\s*[:=]\s*)[^\r\n]+',
                  r'\1[REDACTED]', text)
    text = re.sub(r'(?i)(Bearer\s+)[^\s,;"\']+', r'\1[REDACTED]', text)
    text = re.sub(r'(?i)((?:access[_-]?token|auth[_-]?token|hatch[_-]?token|token|hatch_sess|datr|api[_-]?key|password|secret|credential)["\']?\s*[:=]\s*["\']?)[^\s"\'&,;]+',
                  r'\1[REDACTED]', text)
    return text


def object_rows(rows):
    rows = checked(rows, list)
    if any(not isinstance(row, dict) for row in rows):
        raise BridgeFailure("protocol_error")
    return rows


def history_rows(rows):
    rows = object_rows(rows)
    latest = {}
    for row in rows:
        message_id = required_text(row.get("message_id"))
        if not message_id:
            continue
        seq = row.get("seq")
        seq = seq if isinstance(seq, int) and not isinstance(seq, bool) and seq >= 0 else 0
        raw = row.get("raw") if isinstance(row.get("raw"), dict) else {}
        payload = raw.get("payload") if isinstance(raw.get("payload"), dict) else {}
        message = {
            "message_id": message_id, "seq": seq,
            "role": required_text(row.get("role")), "text": required_text(row.get("text")),
            "timestamp": optional_text(row.get("timestamp")),
            "status": optional_text(row.get("status") or payload.get("status") or raw.get("status")),
        }
        if message_id not in latest or seq >= latest[message_id]["seq"]:
            latest[message_id] = message
    return sorted(latest.values(), key=lambda row: (row["seq"], row["message_id"]))


def secure_directory(path):
    """Open each directory component with O_NOFOLLOW, pinning the final fd."""
    path = Path(path)
    if not path.is_absolute() or ".." in path.parts:
        raise BridgeFailure("invalid_request")
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.parts[1:]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = next_fd
            info = os.fstat(fd)
            if info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022:
                raise BridgeFailure("insecure_config")
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise BridgeFailure("insecure_config")
        result, fd = fd, None
        return result
    finally:
        if fd is not None:
            os.close(fd)


def secure_file(directory_fd, name, private):
    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory_fd)
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_nlink != 1 or info.st_mode & (0o077 if private else 0o022)):
            raise BridgeFailure("insecure_config")
        if info.st_size > MAX_REQUEST:
            raise BridgeFailure("insecure_config")
        result, fd = fd, None
        return result
    finally:
        if fd is not None:
            os.close(fd)


def compatible_cookie_descriptor(source_fd):
    """Adapt HttpOnly Netscape rows in memory, never modifying the user's jar.

    The installed loader skips all # comments, including Netscape's HttpOnly
    prefix. Only that pseudo-comment prefix is removed. Credentials stay inside
    this child; a sealed private memfd keeps subsequent upstream reads pinned.
    """
    data = os.pread(source_fd, MAX_REQUEST + 1, 0)
    if len(data) > MAX_REQUEST:
        raise BridgeFailure("insecure_config")
    data = b"".join(line[len(b"#HttpOnly_"):] if line.startswith(b"#HttpOnly_") else line
                    for line in data.splitlines(keepends=True))
    fd = os.memfd_create("muse-account-session", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(os.dup(fd), "wb") as output:
            output.write(data)
        os.lseek(fd, 0, os.SEEK_SET)
        # Linux UAPI fcntl.h values; standalone Python builds may omit the
        # symbolic constants even when the running kernel supports memfd seals.
        fcntl.fcntl(fd, getattr(fcntl, "F_ADD_SEALS", 1024 + 9),
                    getattr(fcntl, "F_SEAL_WRITE", 0x0008)
                    | getattr(fcntl, "F_SEAL_GROW", 0x0004)
                    | getattr(fcntl, "F_SEAL_SHRINK", 0x0002)
                    | getattr(fcntl, "F_SEAL_SEAL", 0x0001))
        result, fd = fd, None
        return result
    finally:
        if fd is not None:
            os.close(fd)


@contextmanager
def secure_config(config_dir):
    """Read nonsecret settings and pin credentials within this child only.

    Do not call upstream load_config: it writes defaults and trusts environment.
    The HttpOnly compatibility copy is sealed memory, never a persistent file.
    """
    directory_fd = cookie_dir_fd = cookie_fd = source_fd = None
    try:
        config_dir = Path(config_dir)
        directory_fd = secure_directory(config_dir)
        settings = {}
        try:
            settings_fd = secure_file(directory_fd, "config.json", private=False)
        except FileNotFoundError:
            settings_fd = None
        if settings_fd is not None:
            with os.fdopen(settings_fd, encoding="utf-8") as file:
                settings = json.loads(file.read(MAX_REQUEST + 1))
            if not isinstance(settings, dict):
                raise BridgeFailure("invalid_request")
        cookie_path = settings.get("cookies_file", str(config_dir / "cookies.txt"))
        if not isinstance(cookie_path, str) or not cookie_path:
            raise BridgeFailure("invalid_request")
        cookie_path = Path(cookie_path)
        if not cookie_path.is_absolute():
            cookie_path = config_dir / cookie_path
        cookie_dir_fd = secure_directory(cookie_path.parent)
        source_fd = secure_file(cookie_dir_fd, cookie_path.name, private=True)
        cookie_fd = compatible_cookie_descriptor(source_fd)
        config = {"cookies_file": f"/proc/self/fd/{cookie_fd}",
                  "node_id": required_text(settings.get("node_id")) or secrets.token_hex(8)}
        if settings.get("vm_id"):
            config["vm_id"] = required_text(settings["vm_id"])
        yield config
    except FileNotFoundError:
        raise BridgeFailure("auth_required") from None
    except (OSError, UnicodeError):
        raise BridgeFailure("insecure_config") from None
    except (ValueError, TypeError):
        raise BridgeFailure("invalid_request") from None
    finally:
        for fd in (cookie_fd, source_fd, cookie_dir_fd, directory_fd):
            if fd is not None:
                os.close(fd)


def validate_request(request):
    if not isinstance(request, dict):
        raise BridgeFailure("invalid_request")
    op = request.get("op")
    allowed = {"connect": set(), "history": {"thread", "limit"},
               "tasks": set(), "send": {"thread", "message", "device_id"}}
    if not isinstance(op, str) or op not in allowed:
        raise BridgeFailure("invalid_request")
    if set(request) - ({"op", "config_dir"} | allowed[op]):
        raise BridgeFailure("invalid_request")
    directory = request.get("config_dir")
    if not isinstance(directory, str) or not directory.startswith("/") or "\x00" in directory:
        raise BridgeFailure("invalid_request")
    thread = request.get("thread")
    if thread is not None and (not isinstance(thread, str) or not thread.strip()
                               or len(thread) > 4096 or "\x00" in thread):
        raise BridgeFailure("invalid_request")
    if op == "history":
        limit = request.get("limit")
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 200:
            raise BridgeFailure("invalid_request")
    if "device_id" in request:
        device = request["device_id"]
        if not isinstance(device, str) or re.fullmatch(r"[A-Za-z0-9_-]{1,128}", device) is None:
            raise BridgeFailure("invalid_request")
    if op == "send":
        message = request.get("message")
        if not isinstance(message, str) or not message.strip() or len(message.encode("utf-8")) > 512 * 1024:
            raise BridgeFailure("invalid_request")


def community_client(config):
    try:
        from muse_mcp.client import MuseClient
    except (ImportError, OSError):
        raise BridgeFailure("bridge_unavailable") from None
    class AwaitedMuseClient(MuseClient):
        def send(self, message, thread=None, wait_seconds=0, fire_and_forget=True, device_id=None):
            # Keep the Noise connection alive until the server processes the
            # request. Upstream's immediate-close dispatch can drop the write.
            # History below, not this transport response, proves persistence.
            body = {"items": [{"type": "text", "text": message}],
                    "node_id": config["node_id"], "capabilities": {}}
            if device_id is not None:
                body["device_id"] = device_id
            if thread is not None:
                body["session_id"] = thread
            with self._gateway(config) as gateway:
                gateway.request("chat.stream", body=body, timeout=30)
            return {"ok": True, "sent": True}
    return AwaitedMuseClient(config_loader=lambda: config)


def error_response(error):
    return {"ok": False, "error": {"code": error.code, "uncertain": error.uncertain}}


def handle_request(request, client_factory=None):
    try:
        validate_request(request)
        with secure_config(request["config_dir"]) as config:
            client = (client_factory or community_client)(config)
            data = Adapter(client, config).dispatch(request)
            return {"ok": True, "data": data}
    except Exception as exc:
        return error_response(classify_exception(exc))


class Adapter:
    def __init__(self, client, config, confirmation_attempts=3):
        self.client = client
        self.config = config
        self.confirmation_attempts = confirmation_attempts

    def dispatch(self, request):
        self.send_started = False
        try:
            return self._dispatch(request)
        except Exception as exc:
            raise classify_exception(exc, self.send_started) from None

    def _dispatch(self, request):
        if request["op"] == "connect":
            status = checked(self.client.status(), dict)
            if status.get("authenticated") is not True:
                raise BridgeFailure("auth_required")
            if not isinstance(status.get("vm_id"), str) or not status["vm_id"]:
                raise BridgeFailure("protocol_error")
            threads = object_rows(self.client.threads(include_archived=True))
            identity = checked(status.get("identity", {}), dict)
            return {
                "identity_name": required_text(identity.get("name")),
                "vm_id": required_text(status.get("vm_id")),
                "threads": [{
                    "session_id": required_text(row.get("session_id")),
                    "title": required_text(row.get("title")),
                    "archived": bool(row.get("archived", False)),
                    "updated_at": optional_text(row.get("updated_at")),
                } for row in threads],
            }
        if request["op"] == "history":
            return history_rows(self.client.history(thread=request.get("thread"), limit=request["limit"]))
        if request["op"] == "tasks":
            with self.client._gateway(self.config) as gateway:
                schedules = gateway.call_json("tasks.list")["schedules"]
                runs = gateway.call_json("tasks.runs")["runs"]
            return {
                "schedules": [{
                    "id": required_text(row.get("id")), "title": required_text(row.get("title")),
                    "enabled": bool(row.get("enabled", False)),
                    "timezone": optional_text(row.get("timezone")),
                    "next_run_at_utc": row.get("next_run_at_utc"),
                } for row in schedules],
                "runs": [{
                    "run_id": required_text(row.get("run_id")), "job_id": required_text(row.get("job_id")),
                    "status": required_text(row.get("status")),
                    "result_summary": content_text(row.get("result_summary")),
                    "error_text": content_text(row.get("error_text")),
                    "scheduled_for_utc": row.get("scheduled_for_utc"),
                } for row in runs],
            }
        if request["op"] == "send":
            import time
            thread = request.get("thread")
            message = request["message"]
            baseline = history_rows(self.client.history(thread=thread, limit=200))
            baseline_seq = max((r["seq"] for r in baseline), default=0)
            baseline_ids = {r["message_id"] for r in baseline}
            self.send_started = True
            device = {"device_id": request["device_id"]} if "device_id" in request else {}
            result = checked(self.client.send(message=message, thread=thread, fire_and_forget=True, wait_seconds=0, **device), dict)
            if result.get("sent") is not True:
                raise BridgeFailure("protocol_error", uncertain=True)
            for attempt in range(self.confirmation_attempts):
                if attempt:
                    time.sleep(1)
                rows = history_rows(self.client.history(thread=thread, limit=200))
                for row in rows:
                    if (row["seq"] > baseline_seq and row["message_id"] not in baseline_ids
                            and row["role"] == "user" and row["text"] == message):
                        return {"sent": True, "confirmed": True, "message_id": row["message_id"]}
            return {"sent": bool(result.get("sent")), "confirmed": False, "message_id": None}
        raise BridgeFailure("invalid_request")


def parse_pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise BridgeFailure("invalid_request")
        result[key] = value
    return result


def main():
    import sys
    # Quiet both Python and native dependency diagnostics before any imports or
    # credential access. Only the saved response descriptor can reach stdout.
    response_fd = os.dup(sys.stdout.fileno())
    with open(os.devnull, "w") as quiet:
        os.dup2(quiet.fileno(), sys.stdout.fileno())
        os.dup2(quiet.fileno(), sys.stderr.fileno())
    request = None
    try:
        raw = sys.stdin.buffer.read(MAX_REQUEST + 1)
        if len(raw) > MAX_REQUEST:
            raise BridgeFailure("invalid_request")
        try:
            request = json.loads(raw, object_pairs_hook=parse_pairs)
        except (ValueError, UnicodeError):
            raise BridgeFailure("invalid_request") from None
        response = handle_request(request)
        encoded = json.dumps(response, ensure_ascii=True, allow_nan=False).encode("utf-8")
        if len(encoded) > MAX_RESPONSE:
            encoded = json.dumps(error_response(BridgeFailure("response_too_large",
                isinstance(request, dict) and request.get("op") == "send"))).encode("utf-8")
    except Exception as exc:
        encoded = json.dumps(error_response(classify_exception(exc))).encode("utf-8")
    try:
        with os.fdopen(response_fd, "wb") as output:
            output.write(encoded + b"\n")
    except OSError:
        pass


if __name__ == "__main__":
    main()
