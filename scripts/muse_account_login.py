#!/usr/bin/env python3
"""Explicit browser sign-in; stdout is a single secret-free JSON result."""
import base64
import hashlib
import hmac
import json
import math
import os
import re
import secrets
import shutil
import signal
import socket
import stat
import struct
import subprocess
from pathlib import Path
import sys
import time

MESSAGES = {
    "invalid_request": "A valid sign-in request is required.",
    "unsafe_config": "Choose a private app configuration directory without existing unrelated credentials.",
    "missing_browser": "Install a Chromium-compatible browser or select its executable.",

    "browser_launch_failed": "The dedicated sign-in browser could not be started.",
    "browser_closed": "The sign-in browser was closed before sign-in completed.",
    "unsafe_devtools": "The dedicated browser debugging connection could not be verified.",
    "devtools_error": "The dedicated browser connection failed. Please retry sign-in.",
    "timeout": "Sign-in timed out. Please retry when ready.",
    "cancelled": "Sign-in was cancelled.",
    "cleanup_failed": "Close the dedicated sign-in browser before retrying.",

    "internal_error": "Sign-in could not be completed. Please retry.",
}


class LoginError(Exception):
    def __init__(self, code):
        self.code = code
        super().__init__(code)


def validate_request(request):
    if not isinstance(request, dict) or not set(request) <= {"config_dir", "browser", "timeout_seconds"}:
        raise LoginError("invalid_request")
    path = request.get("config_dir")
    browser = request.get("browser")
    timeout = request.get("timeout_seconds", 300)
    if (not isinstance(path, str) or not os.path.isabs(path)
            or len(path) > 4096 or "\x00" in path
            or ".." in path.split("/") or path == "/"
            or (browser is not None and (not isinstance(browser, str) or not browser
                or len(browser) > 4096 or "\x00" in browser))
            or isinstance(timeout, bool) or not isinstance(timeout, int)
            or not 1 <= timeout <= 900):
        raise LoginError("invalid_request")
    return {"config_dir": path, "browser": browser, "timeout_seconds": timeout}


JAR_MARKER = b"# Unofficial Muse for Linux isolated sign-in\n"


def select_cookie(cookies):
    """Narrow a URL-scoped CDP response; never persist other cookies."""
    if not isinstance(cookies, list):
        return None
    candidates = []
    for cookie in cookies:
        if not isinstance(cookie, dict):
            continue
        value = cookie.get("value")
        expires = cookie.get("expires", -1)
        if (cookie.get("name") != "hatch_sess"
                or cookie.get("domain") not in ("muse.ai", ".muse.ai")
                or cookie.get("path") != "/" or cookie.get("secure") is not True
                or "partitionKey" in cookie or cookie.get("partitionKeyOpaque")
                or not isinstance(value, str) or len(value) > 16384
                or not re.fullmatch(r"[\x21\x23-\x2b\x2d-\x3a\x3c-\x5b\x5d-\x7e]+", value)
                or isinstance(expires, bool) or not isinstance(expires, (int, float))
                or not math.isfinite(expires)
                or (expires != -1 and expires <= time.time())):
            continue
        candidates.append({"value": value, "httpOnly": cookie.get("httpOnly") is True,
                           "expires": 0 if expires == -1 else int(expires)})
    # Do not guess between two conflicting cookies with the same name.
    return candidates[0] if len(candidates) == 1 else None


def open_config_dir(path):
    """Walk with directory FDs, rejecting symlinks and unsafe ownership."""
    validate_request({"config_dir": path})
    parts = [part for part in path.split("/") if part and part != "."]
    if not parts or parts[-1] == "muse-cli":
        raise LoginError("unsafe_config")
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for index, part in enumerate(parts):
            try:
                os.mkdir(part, 0o700, dir_fd=fd)
            except FileExistsError:
                pass
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
            info = os.fstat(fd)
            if info.st_uid not in (0, os.geteuid()):
                raise LoginError("unsafe_config")
            if info.st_mode & 0o022 and not (info.st_uid == 0 and info.st_mode & stat.S_ISVTX):
                raise LoginError("unsafe_config")
            if index == len(parts) - 1 and (info.st_uid != os.geteuid() or info.st_mode & 0o077):
                raise LoginError("unsafe_config")
        return fd
    except OSError:
        os.close(fd)
        raise LoginError("unsafe_config") from None
    except BaseException:
        os.close(fd)
        raise


def check_existing_jar(directory):
    try:
        fd = os.open("cookies.txt", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
    except FileNotFoundError:
        return
    except OSError:
        raise LoginError("unsafe_config") from None
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                or info.st_nlink != 1 or info.st_mode & 0o077
                or os.read(fd, 128).splitlines()[:2] != [b"# Netscape HTTP Cookie File", JAR_MARKER.rstrip()]):
            raise LoginError("unsafe_config")
    finally:
        os.close(fd)


def save_cookie(directory, cookie):
    """Replace only our own jar; files and fsync use the validated directory FD."""
    check_existing_jar(directory)
    prefix = "#HttpOnly_" if cookie["httpOnly"] else ""
    data = (b"# Netscape HTTP Cookie File\n" + JAR_MARKER
            + (prefix + "muse.ai\tFALSE\t/\tTRUE\t" + str(cookie["expires"])
               + "\thatch_sess\t" + cookie["value"] + "\n").encode("ascii"))
    temporary = ".cookies-" + secrets.token_hex(16)
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                 0o600, dir_fd=directory)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        check_existing_jar(directory)
        os.replace(temporary, "cookies.txt", src_dir_fd=directory, dst_dir_fd=directory)
        os.fsync(directory)
    finally:
        try:
            os.unlink(temporary, dir_fd=directory)
        except FileNotFoundError:
            pass


def validate_cdp_url(url):
    if not isinstance(url, str):
        raise LoginError("unsafe_devtools")
    match = re.fullmatch(r"ws://127\.0\.0\.1:([1-9][0-9]{0,4})/devtools/browser/[A-Za-z0-9-]{1,128}", url)
    if not match or not 1 <= int(match.group(1)) <= 65535:
        raise LoginError("unsafe_devtools")
    return int(match.group(1))


def validate_listener(port, process):
    """Linux fail-closed proof: loopback listener is owned by our process tree."""
    if process.poll() is not None:
        raise LoginError("browser_closed")
    inodes = set()
    try:
        for table, allowed in (("tcp", "0100007F"),
                               ("tcp6", "00000000000000000000000001000000")):
            for line in Path("/proc/net/" + table).read_text().splitlines()[1:]:
                fields = line.split()
                address, number = fields[1].split(":")
                if int(number, 16) == port and fields[3] == "0A":
                    if address != allowed or int(fields[7]) != os.geteuid():
                        raise LoginError("unsafe_devtools")
                    if table == "tcp":
                        inodes.add(fields[9])
        if not inodes:
            raise LoginError("unsafe_devtools")
        parents = {}
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                if entry.stat().st_uid == os.geteuid():
                    fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
                    parents[int(entry.name)] = int(fields[1])
            except (OSError, ValueError, IndexError):
                continue
        for pid in parents:
            ancestor, seen = pid, set()
            while ancestor != process.pid and ancestor in parents and ancestor not in seen:
                seen.add(ancestor)
                ancestor = parents[ancestor]
            if ancestor != process.pid:
                continue
            try:
                for entry in Path(f"/proc/{pid}/fd").iterdir():
                    try:
                        target = os.readlink(entry)
                    except OSError:
                        continue
                    if target.startswith("socket:[") and target[8:-1] in inodes:
                        if process.poll() is None:
                            return
            except OSError:
                continue
    except (OSError, ValueError, IndexError):
        raise LoginError("unsafe_devtools") from None
    raise LoginError("unsafe_devtools")


def read_devtools_endpoint(profile, process, launched_ns):
    if process.poll() is not None:
        raise LoginError("browser_closed")
    try:
        fd = os.open("DevToolsActivePort", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                     dir_fd=profile)
    except FileNotFoundError:
        return None
    except OSError:
        raise LoginError("unsafe_devtools") from None
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                or info.st_nlink != 1 or info.st_mode & 0o022
                or info.st_mtime_ns < launched_ns or info.st_ctime_ns < launched_ns
                or not 1 <= info.st_size <= 512):
            raise LoginError("unsafe_devtools")
        try:
            lines = os.read(fd, 513).decode("ascii").splitlines()
        except UnicodeError:
            raise LoginError("unsafe_devtools") from None
        if len(lines) != 2:
            raise LoginError("unsafe_devtools")
        url = "ws://127.0.0.1:" + lines[0] + lines[1]
        port = validate_cdp_url(url)
        validate_listener(port, process)
        return url
    finally:
        os.close(fd)


NATIVE_BROWSERS = ("google-chrome", "google-chrome-stable", "chromium", "chromium-browser",
                   "brave-browser", "brave", "microsoft-edge", "microsoft-edge-stable", "vivaldi", "vivaldi-stable")
FLATPAK_BROWSERS = ("com.brave.Browser", "com.microsoft.Edge", "com.vivaldi.Vivaldi", "org.chromium.Chromium")


def find_browser(requested):
    """Only explicit executable paths/names, never shell strings or extra flags."""
    if requested and not requested.startswith("flatpak:"):
        candidate = shutil.which(requested)
        if candidate and (os.path.isabs(requested) or requested in NATIVE_BROWSERS):
            return [candidate]
        raise LoginError("missing_browser")
    if requested is None:
        for name in NATIVE_BROWSERS:
            candidate = shutil.which(name)
            if candidate:
                return [candidate]
    flatpak = shutil.which("flatpak")
    ids = FLATPAK_BROWSERS if requested is None else (requested.removeprefix("flatpak:"),)
    if flatpak:
        for appid in ids:
            if appid not in FLATPAK_BROWSERS:
                continue
            try:
                found = subprocess.run([flatpak, "info", appid], stdin=subprocess.DEVNULL,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                       timeout=3, check=False)
            except (OSError, subprocess.TimeoutExpired):
                continue
            if found.returncode == 0:
                return [flatpak, appid]
    raise LoginError("missing_browser")


def browser_command(browser, profile):
    flags = ["--user-data-dir=" + profile, "--remote-debugging-address=127.0.0.1",
             "--remote-debugging-port=0", "--no-first-run", "--no-default-browser-check",
             "--disable-extensions", "--disable-sync", "--disable-background-networking",
             "--disable-breakpad", "--disable-crash-reporter", "--new-window"]
    if len(browser) == 2 and browser[1] in FLATPAK_BROWSERS:
        command = [browser[0], "run", "--die-with-parent",
                   "--nofilesystem=host:reset", "--filesystem=" + profile, "--share=network"]
        for key, leaf in (("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                          ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state")):
            command.append("--env=" + key + "=" + profile + "/" + leaf)
        command.append(browser[1])
    else:
        command = list(browser)
    return command + flags + ["https://muse.ai/"]


def remaining(deadline):
    value = deadline - time.monotonic()
    if value <= 0:
        raise LoginError("timeout")
    return value


class LoopbackWebSocket:
    """Bounded RFC 6455 text/control transport over an already verified socket.

    No third-party runtime, DNS, HTTP proxy, redirects, compression or cookies.
    Only the CDP text-message subset is accepted; unsupported frames fail closed.
    """
    LIMIT = 1048576

    def __init__(self, raw, url, deadline):
        port = validate_cdp_url(url)
        self.raw = raw
        self.deadline = deadline
        self.timeout = 2
        nonce = base64.b64encode(secrets.token_bytes(16)).decode("ascii")
        path = url.split(f"127.0.0.1:{port}", 1)[1]
        request = (f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
                   "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                   f"Sec-WebSocket-Key: {nonce}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        self._timeout()
        raw.sendall(request.encode("ascii"))
        response = bytearray()
        while not response.endswith(b"\r\n\r\n"):
            if len(response) >= 16384:
                raise LoginError("devtools_error")
            response.extend(self._exact(1))
        try:
            lines = response.decode("ascii").split("\r\n")
            if lines[0].split(" ", 2)[:2] != ["HTTP/1.1", "101"]:
                raise LoginError("devtools_error")
            headers = {}
            for line in lines[1:-2]:
                name, value = line.split(":", 1)
                name = name.lower()
                if not name or name in headers or name.strip() != name:
                    raise LoginError("devtools_error")
                headers[name] = value.strip()
            # SHA-1 is required by the WebSocket handshake, not used for secrets.
            digest = hashlib.sha1((nonce + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii"),
                                  usedforsecurity=False).digest()
            expected = base64.b64encode(digest).decode("ascii")
            if (headers.get("upgrade", "").lower() != "websocket"
                    or "upgrade" not in {part.strip().lower() for part in headers.get("connection", "").split(",")}
                    or not hmac.compare_digest(headers.get("sec-websocket-accept", ""), expected)
                    or "sec-websocket-extensions" in headers or "sec-websocket-protocol" in headers):
                raise LoginError("devtools_error")
        except (ValueError, UnicodeError):
            raise LoginError("devtools_error") from None

    def settimeout(self, timeout):
        self.timeout = timeout

    def _timeout(self):
        self.raw.settimeout(min(self.timeout, remaining(self.deadline)))

    def _exact(self, length):
        chunks = bytearray()
        while len(chunks) < length:
            self._timeout()
            chunk = self.raw.recv(length - len(chunks))
            if not chunk:
                raise LoginError("devtools_error")
            chunks.extend(chunk)
        return bytes(chunks)

    def _send(self, payload, opcode):
        if len(payload) > self.LIMIT:
            raise LoginError("devtools_error")
        mask = secrets.token_bytes(4)
        header = bytes([0x80 | opcode])
        if len(payload) < 126:
            header += bytes([0x80 | len(payload)])
        elif len(payload) <= 65535:
            header += bytes([0x80 | 126]) + struct.pack("!H", len(payload))
        else:
            header += bytes([0x80 | 127]) + struct.pack("!Q", len(payload))
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self._timeout()
        self.raw.sendall(header + mask + masked)

    def send(self, text):
        self._send(text.encode("utf-8"), 1)

    def recv(self):
        message = bytearray()
        started = False
        while True:
            first, second = self._exact(2)
            final, opcode = bool(first & 0x80), first & 0x0f
            length = second & 0x7f
            if first & 0x70 or second & 0x80 or opcode not in (0, 1, 8, 9, 10):
                raise LoginError("devtools_error")
            if length == 126:
                length = struct.unpack("!H", self._exact(2))[0]
                if length < 126:
                    raise LoginError("devtools_error")
            elif length == 127:
                length = struct.unpack("!Q", self._exact(8))[0]
                if length <= 65535:
                    raise LoginError("devtools_error")
            if length > self.LIMIT or (opcode >= 8 and (not final or length > 125)):
                raise LoginError("devtools_error")
            payload = self._exact(length)
            if opcode == 8:
                raise LoginError("devtools_error")
            if opcode == 9:
                self._send(payload, 10)
                continue
            if opcode == 10:
                continue
            if (opcode == 0 and not started) or (opcode == 1 and started):
                raise LoginError("devtools_error")
            started = True
            if len(message) + length > self.LIMIT:
                raise LoginError("devtools_error")
            message.extend(payload)
            if final:
                try:
                    return message.decode("utf-8")
                except UnicodeError:
                    raise LoginError("devtools_error") from None

    def close(self, timeout=0.1):
        try:
            self.timeout = timeout
            self._send(struct.pack("!H", 1000), 8)
        except Exception:
            pass
        finally:
            self.raw.close()


class CDP:
    """A single process-validated loopback socket, no DNS/proxy/redirects."""
    def __init__(self, url, process, deadline):
        port = validate_cdp_url(url)
        validate_listener(port, process)
        self.deadline = deadline
        self.process = process
        self.sequence = 0
        self.socket = None
        raw = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            raw.settimeout(min(2, remaining(deadline)))
            raw.connect(("127.0.0.1", port))
            if raw.getpeername() != ("127.0.0.1", port):
                raise LoginError("unsafe_devtools")
            validate_listener(port, process)
            self.socket = LoopbackWebSocket(raw, url, deadline)
            validate_listener(port, process)
        except LoginError:
            raw.close()
            raise
        except Exception:
            raw.close()
            raise LoginError("devtools_error") from None

    def call(self, method, params=None, session=None):
        if self.process.poll() is not None:
            raise LoginError("browser_closed")
        self.sequence += 1
        request = {"id": self.sequence, "method": method, "params": params or {}}
        if session is not None:
            request["sessionId"] = session
        try:
            self.socket.settimeout(min(2, remaining(self.deadline)))
            self.socket.send(json.dumps(request))
            while True:
                self.socket.settimeout(min(2, remaining(self.deadline)))
                message = self.socket.recv()
                if not isinstance(message, str) or len(message) > 1048576:
                    raise LoginError("devtools_error")
                response = json.loads(message)
                if not isinstance(response, dict):
                    raise LoginError("devtools_error")
                if response.get("id") != self.sequence:
                    continue
                if "error" in response or not isinstance(response.get("result"), dict):
                    raise LoginError("devtools_error")
                return response["result"]
        except LoginError:
            raise
        except Exception:
            remaining(self.deadline)
            if self.process.poll() is not None:
                raise LoginError("browser_closed") from None
            raise LoginError("devtools_error") from None

    def close(self):
        if self.socket is not None:
            try:
                self.socket.close(timeout=0.1)
            except Exception:
                pass

    def read_cookie(self):
        targets = self.call("Target.getTargets").get("targetInfos", [])
        if not isinstance(targets, list):
            raise LoginError("devtools_error")
        for target in targets:
            if not isinstance(target, dict) or target.get("type") != "page":
                continue
            url = target.get("url")
            if not isinstance(url, str) or not re.fullmatch(r"https://muse\.ai(?::443)?/[^\x00-\x20\\]*", url):
                continue
            target_id = target.get("targetId")
            if not isinstance(target_id, str):
                continue
            session = self.call("Target.attachToTarget", {"targetId": target_id, "flatten": True}).get("sessionId")
            if not isinstance(session, str):
                raise LoginError("devtools_error")
            try:
                response = self.call("Network.getCookies", {"urls": ["https://muse.ai/"]}, session)
                cookie = select_cookie(response.get("cookies"))
            finally:
                self.call("Target.detachFromTarget", {"sessionId": session})
            if cookie is not None:
                return cookie
        return None


def process_group_alive(pgid):
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
            if int(fields[2]) == pgid and fields[0] != "Z":
                return True
        except (OSError, ValueError, IndexError):
            continue
    return False


def stop_browser(process):
    if process is None:
        return
    # Only Popen(start_new_session=True) processes are passed here.
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        limit = time.monotonic() + 2
        while time.monotonic() < limit:
            process.poll()  # Reap the direct child before checking for survivors.
            if not process_group_alive(process.pid):
                process.wait(timeout=1)
                return
            time.sleep(0.03)
    raise LoginError("cleanup_failed")


def login(request):
    request = validate_request(request)
    browser = find_browser(request["browser"])
    directory = open_config_dir(request["config_dir"])
    try:
        return login_in_directory(request, browser, directory)
    finally:
        os.close(directory)


def login_in_directory(request, browser, directory):
    profile_name = ".login-" + secrets.token_hex(16)
    profile_fd = None
    process = None
    client = None
    created = False
    try:
        check_existing_jar(directory)
        os.mkdir(profile_name, 0o700, dir_fd=directory)
        created = True
        profile_fd = os.open(profile_name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
        profile = os.path.join(request["config_dir"], profile_name)
        # Profile is always new and private, never any installed browser's profile.
        environment = dict(os.environ)
        for key in ("CHROME_LOG_FILE", "SSLKEYLOGFILE", "CHROME_REMOTE_DESKTOP_SESSION",
                    "CHROME_USER_DATA_DIR", "CHROME_CONFIG_HOME", "CHROMIUM_USER_FLAGS",
                    "CHROMIUM_FLAGS", "CHROME_FLAGS", "BRAVE_FLAGS", "EDGE_FLAGS",
                    "VIVALDI_FLAGS", "VIVALDI_USER_FLAGS"):
            environment.pop(key, None)
        # Flatpak's outer launcher still needs the host's XDG installation
        # directories; browser_command redirects XDG inside its sandbox instead.
        if not (len(browser) == 2 and browser[1] in FLATPAK_BROWSERS):
            for key, leaf in (("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                              ("XDG_DATA_HOME", "data"), ("XDG_STATE_HOME", "state")):
                environment[key] = os.path.join(profile, leaf)
        deadline = time.monotonic() + request["timeout_seconds"]
        launched_ns = time.time_ns()
        try:
            process = subprocess.Popen(browser_command(browser, profile), stdin=subprocess.DEVNULL,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                       start_new_session=True, close_fds=True, env=environment)
        except OSError:
            raise LoginError("browser_launch_failed") from None
        while True:
            remaining(deadline)
            endpoint = read_devtools_endpoint(profile_fd, process, launched_ns)
            if endpoint is not None:
                break
            time.sleep(min(0.05, remaining(deadline)))
        client = CDP(endpoint, process, deadline)
        while True:
            remaining(deadline)
            cookie = client.read_cookie()
            if cookie is not None:
                break
            time.sleep(min(0.25, remaining(deadline)))
    finally:
        try:
            if client is not None:
                client.close()
            stop_browser(process)
            if created:
                shutil.rmtree(profile_name, dir_fd=directory)
        finally:
            if profile_fd is not None:
                os.close(profile_fd)
    save_cookie(directory, cookie)
    return {"ok": True, "cookie_file": os.path.join(request["config_dir"], "cookies.txt"),
            "message": "Account sign-in completed."}


def main(stdin=None, stdout=None):
    stdin = sys.stdin if stdin is None else stdin
    stdout = sys.stdout if stdout is None else stdout
    try:
        raw = stdin.read(16385)
        if len(raw) > 16384:
            raise LoginError("invalid_request")
        try:
            request = json.loads(raw)
        except (ValueError, TypeError):
            raise LoginError("invalid_request") from None
        result = login(request)
    except LoginError as error:
        code = error.code if error.code in MESSAGES else "internal_error"
        result = {"ok": False, "error": code, "message": MESSAGES[code]}
    except KeyboardInterrupt:
        result = {"ok": False, "error": "cancelled", "message": MESSAGES["cancelled"]}
    except Exception:
        result = {"ok": False, "error": "internal_error",
                  "message": MESSAGES["internal_error"]}
    stdout.write(json.dumps(result) + "\n")
    stdout.flush()
    return 0 if result.get("ok") else 1


def cancel_signal(_signum, _frame):
    # The UI and its worker may both request cancellation. A second signal
    # must not raise inside the first cancellation's browser/profile cleanup.
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    raise LoginError("cancelled")


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, cancel_signal)
    signal.signal(signal.SIGINT, cancel_signal)
    sys.exit(main())
