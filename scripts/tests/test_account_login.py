"""Offline tests: never launch a real browser or read a user's cookies."""
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import socket
import signal
import subprocess
import struct
import sys
import time
import unittest
from unittest.mock import patch

FAKE_BROWSER = r'''
import base64, hashlib, json, os, signal, socket, struct, sys, time
from pathlib import Path
profile = Path(next(a.split('=', 1)[1] for a in sys.argv if a.startswith('--user-data-dir=')))
Path(os.environ['MUSE_FAKE_PID']).write_text(str(os.getpid()))
if os.environ.get('MUSE_FAKE_MODE') == 'cancel_twice':
    def delayed_shutdown(*_):
        Path(os.environ['MUSE_FAKE_STOPPING']).write_text('stopping')
        time.sleep(0.5)
        sys.exit(0)
    signal.signal(signal.SIGTERM, delayed_shutdown)
server = socket.socket()
server.bind(('127.0.0.1', 0))
server.listen()
(profile / 'DevToolsActivePort').write_text(str(server.getsockname()[1]) + '\n/devtools/browser/fake-browser-token\n')
connection, peer = server.accept()
if os.environ.get('MUSE_FAKE_MODE') == 'redirect':
    connection.sendall(b'HTTP/1.1 302 Found\r\nLocation: ws://127.0.0.1:1/forbidden-redirect\r\nContent-Length: 0\r\n\r\n')
    connection.close()
    time.sleep(30)
    sys.exit(0)
connection.settimeout(10)
def exact(length):
    data = b''
    while len(data) < length:
        chunk = connection.recv(length - len(data))
        if not chunk: raise EOFError
        data += chunk
    return data
headers = b''
while b'\r\n\r\n' not in headers:
    headers += exact(1)
key = next(line.split(b':', 1)[1].strip() for line in headers.split(b'\r\n') if line.lower().startswith(b'sec-websocket-key:'))
accept = base64.b64encode(hashlib.sha1(key + b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
if os.environ.get('MUSE_FAKE_MODE') == 'bad-handshake': accept = b'incorrect-accept'
connection.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + b'\r\n\r\n')
def send(value):
    payload = json.dumps(value).encode()
    header = bytes([0x81, len(payload)]) if len(payload) < 126 else bytes([0x81, 126]) + struct.pack('!H', len(payload))
    connection.sendall(header + payload)
try:
    while True:
        a, b = exact(2)
        length = b & 127
        if length == 126: length = struct.unpack('!H', exact(2))[0]
        elif length == 127: length = struct.unpack('!Q', exact(8))[0]
        mask = exact(4) if b & 128 else b''
        payload = exact(length)
        if mask: payload = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        if a & 15 == 8: break
        request = json.loads(payload)
        with open(os.environ['MUSE_FAKE_REQUESTS'], 'a') as log:
            log.write(json.dumps(request) + '\n')
        method = request['method']
        if os.environ.get('MUSE_FAKE_MODE') == 'error':
            send({'id': request['id'], 'error': {'message': 'offline-fake-session-sensitive'}})
            continue
        if method == 'Target.getTargets':
            result = {'targetInfos': [
                {'type': 'page', 'targetId': 'evil', 'url': 'https://muse.ai.evil.test/'},
                {'type': 'page', 'targetId': 'official', 'url': 'https://muse.ai/'}]}
        elif method == 'Target.attachToTarget': result = {'sessionId': 'fake-session'}
        elif method == 'Network.getCookies':
            cookie = {'name': 'hatch_sess', 'domain': '.muse.ai', 'path': '/', 'secure': True,
                      'httpOnly': True, 'expires': -1, 'value': 'offline-fake-session-sensitive'}
            result = {'cookies': [] if os.environ.get('MUSE_FAKE_MODE') in ('timeout', 'cancel', 'cancel_twice') else [cookie]}
        else: result = {}
        # Asynchronous events must not confuse request/response IDs.
        send({'method': 'irrelevant.event', 'params': {'value': 'never-log-event-sensitive'}})
        send({'id': request['id'], 'result': result})
except (EOFError, OSError):
    pass
connection.close()
server.close()
time.sleep(30)
'''

SCRIPT = Path(__file__).resolve().parents[1] / "muse_account_login.py"


def load_helper():
    if not SCRIPT.exists():
        raise AssertionError("explicit browser sign-in helper is not implemented")
    spec = importlib.util.spec_from_file_location("muse_account_login", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class LoginTests(unittest.TestCase):
    def test_json_entrypoint_never_reflects_exception_or_request_secrets(self):
        helper = load_helper()
        secret = "sentinel-sensitive-session-value"
        output = io.StringIO()
        with patch.object(helper, "login", side_effect=RuntimeError(secret)):
            code = helper.main(io.StringIO(json.dumps({"config_dir": "/irrelevant"})), output)
        self.assertEqual(code, 1)
        self.assertEqual(json.loads(output.getvalue()), {
            "ok": False, "error": "internal_error",
            "message": "Sign-in could not be completed. Please retry.",
        })
        self.assertNotIn(secret, output.getvalue())
        self.assertEqual(len(output.getvalue().splitlines()), 1)

    def test_requests_are_strict_bounded_and_never_echo_invalid_fields(self):
        helper = load_helper()
        for request in (None, [], {}, {"config_dir": "relative"},
                        {"config_dir": "/safe", "cookie": "secret"},
                        {"config_dir": "/safe", "timeout_seconds": -1},
                        {"config_dir": "/safe", "browser": ["--bad"]}):
            with self.subTest(request=request):
                with self.assertRaises(helper.LoginError) as error:
                    helper.validate_request(request)
                self.assertEqual(error.exception.code, "invalid_request")
        request = helper.validate_request({"config_dir": "/safe"})
        self.assertEqual(request["timeout_seconds"], 300)
        output = io.StringIO()
        self.assertEqual(helper.main(io.StringIO("x" * 17000), output), 1)
        self.assertEqual(json.loads(output.getvalue())["error"], "invalid_request")

    def test_only_exact_muse_session_is_saved_as_private_atomic_jar(self):
        helper = load_helper()
        secret = "test-session-not-a-real-credential"
        cookie = {"name": "hatch_sess", "value": secret, "domain": ".muse.ai",
                  "path": "/", "secure": True, "httpOnly": True, "expires": -1}
        bad = [dict(cookie, domain="evil-muse.ai"), dict(cookie, domain="muse.ai.evil"),
               dict(cookie, domain="login.muse.ai"), dict(cookie, name="other"),
               dict(cookie, value="injected\nrow"), dict(cookie, secure=False),
               dict(cookie, path="/private"), dict(cookie, partitionKey={"topLevelSite": "https://other.test"})]
        self.assertIsNone(helper.select_cookie(bad))
        selected = helper.select_cookie(bad + [cookie])
        with tempfile.TemporaryDirectory() as root:
            config = Path(root) / "app"
            fd = helper.open_config_dir(str(config))
            try:
                helper.save_cookie(fd, selected)
                jar = config / "cookies.txt"
                text = jar.read_text()
                self.assertEqual(jar.stat().st_mode & 0o777, 0o600)
                self.assertEqual(config.stat().st_mode & 0o777, 0o700)
                self.assertEqual([line for line in text.splitlines()
                                  if line and not line.startswith("# ")
                                  and not line.startswith("# Netscape")],
                                 ["#HttpOnly_muse.ai\tFALSE\t/\tTRUE\t0\thatch_sess\t" + secret])
                helper.save_cookie(fd, dict(selected, value="replacement"))
                self.assertNotIn(secret, jar.read_text())
                self.assertEqual(sorted(p.name for p in config.iterdir()), ["cookies.txt"])
            finally:
                os.close(fd)

    def test_directory_and_jar_symlinks_permissions_and_unrelated_files_are_refused(self):
        helper = load_helper()
        cookie = {"value": "fake", "httpOnly": True, "expires": 0}
        with tempfile.TemporaryDirectory() as root:
            base = Path(root)
            actual = base / "actual"
            actual.mkdir(mode=0o700)
            (base / "alias").symlink_to(actual, target_is_directory=True)
            for path in (base / "alias", base / "muse-cli"):
                with self.assertRaises(helper.LoginError):
                    helper.open_config_dir(str(path))
            actual.chmod(0o755)
            with self.assertRaises(helper.LoginError):
                helper.open_config_dir(str(actual))
            actual.chmod(0o700)
            fd = helper.open_config_dir(str(actual))
            try:
                unrelated = base / "unrelated"
                unrelated.write_text("never overwrite")
                (actual / "cookies.txt").symlink_to(unrelated)
                with self.assertRaises(helper.LoginError):
                    helper.save_cookie(fd, cookie)
                self.assertEqual(unrelated.read_text(), "never overwrite")
                (actual / "cookies.txt").unlink()
                (actual / "cookies.txt").write_text("existing other client credential")
                (actual / "cookies.txt").chmod(0o600)
                with self.assertRaises(helper.LoginError):
                    helper.save_cookie(fd, cookie)
                self.assertEqual((actual / "cookies.txt").read_text(), "existing other client credential")
            finally:
                os.close(fd)

    def test_devtools_file_is_fresh_nofollow_and_scoped_to_live_process_listener(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as root, socket.socket() as listener:
            profile = Path(root)
            fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            port = listener.getsockname()[1]
            launched = time.time_ns()
            active = profile / "DevToolsActivePort"
            active.write_text(str(port) + "\n/devtools/browser/fake-process-token\n")
            process = type("Process", (), {"pid": os.getpid(), "poll": lambda self: None})()
            try:
                url = helper.read_devtools_endpoint(fd, process, launched)
                self.assertEqual(url, f"ws://127.0.0.1:{port}/devtools/browser/fake-process-token")
                os.utime(active, ns=(launched - 1000000000, launched - 1000000000))
                with self.assertRaises(helper.LoginError):
                    helper.read_devtools_endpoint(fd, process, launched)
                active.unlink()
                target = profile / "other"
                target.write_text(str(port) + "\n/devtools/browser/fake\n")
                active.symlink_to(target)
                with self.assertRaises(helper.LoginError):
                    helper.read_devtools_endpoint(fd, process, launched)
                active.unlink()
                active.write_text(str(port) + "\n/devtools/browser/fake\n")
                with subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"]) as stranger:
                    try:
                        with self.assertRaises(helper.LoginError):
                            helper.read_devtools_endpoint(fd, stranger, launched)
                    finally:
                        stranger.terminate()
                        stranger.wait(timeout=5)
            finally:
                os.close(fd)

    def test_cdp_urls_cannot_escape_loopback_or_include_ambiguous_authorities(self):
        helper = load_helper()
        for url in ("ws://evil.test:123/devtools/browser/fake", "ws://127.0.0.1.evil:123/devtools/browser/fake",
                    "ws://user@127.0.0.1:123/devtools/browser/fake", "ws://localhost:123/devtools/browser/fake",
                    "ws://127.0.0.1:123/devtools/browser/fake?redirect=evil", "wss://127.0.0.1:123/devtools/browser/fake",
                    "ws://[::ffff:127.0.0.1]:123/devtools/browser/fake"):
            with self.subTest(url=url), self.assertRaises(helper.LoginError):
                helper.validate_cdp_url(url)
        self.assertEqual(helper.validate_cdp_url("ws://127.0.0.1:123/devtools/browser/fake"), 123)

    def test_browser_command_has_exact_official_url_and_never_reuses_default_profile(self):
        helper = load_helper()
        profile = "/private/app/.login-random"
        native = helper.browser_command(["/usr/bin/chromium"], profile)
        self.assertEqual(native[-1], "https://muse.ai/")
        self.assertIn("--user-data-dir=" + profile, native)
        self.assertIn("--remote-debugging-address=127.0.0.1", native)
        self.assertIn("--remote-debugging-port=0", native)
        self.assertNotIn("--no-sandbox", native)
        flatpak = helper.browser_command(["/usr/bin/flatpak", "com.brave.Browser"], profile)
        self.assertEqual(flatpak[-1], "https://muse.ai/")
        self.assertIn("--nofilesystem=host:reset", flatpak)
        self.assertIn("--filesystem=" + profile, flatpak)
        # flatpak run has no --new-instance option; the fresh --user-data-dir
        # is what prevents Chromium from routing to an existing browser.
        self.assertNotIn("--new-instance", flatpak)
        self.assertIn("--die-with-parent", flatpak)
        with patch.object(helper.shutil, "which", return_value=None):
            with self.assertRaises(helper.LoginError) as error:
                helper.find_browser(None)
        self.assertEqual(error.exception.code, "missing_browser")

    def run_fake(self, mode="success"):
        with tempfile.TemporaryDirectory() as root:
            base = Path(root)
            browser = base / "fake-browser"
            browser.write_text("#!" + sys.executable + "\n" + FAKE_BROWSER)
            browser.chmod(0o700)
            log = base / "requests.jsonl"
            config = base / "account"
            pid_file = base / "browser.pid"
            stopping_file = base / "browser.stopping"
            environment = dict(os.environ, MUSE_FAKE_REQUESTS=str(log), MUSE_FAKE_MODE=mode,
                               MUSE_FAKE_PID=str(pid_file), MUSE_FAKE_STOPPING=str(stopping_file))
            request = json.dumps({"config_dir": str(config), "browser": str(browser),
                                  "timeout_seconds": 1 if mode == "timeout" else 5})
            if mode in ("cancel", "cancel_twice"):
                with subprocess.Popen([sys.executable, str(SCRIPT)], stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                      text=True, env=environment) as child:
                    assert child.stdin is not None
                    child.stdin.write(request)
                    child.stdin.close()
                    child.stdin = None
                    deadline = time.monotonic() + 5
                    while not log.exists() and child.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.02)
                    child.terminate()
                    if mode == "cancel_twice":
                        deadline = time.monotonic() + 3
                        while not stopping_file.exists() and child.poll() is None and time.monotonic() < deadline:
                            time.sleep(0.01)
                        self.assertTrue(stopping_file.exists(), "browser cleanup never started")
                        child.send_signal(signal.SIGINT)
                    out, err = child.communicate(timeout=8)
                    completed = subprocess.CompletedProcess(child.args, child.returncode, out, err)
            else:
                completed = subprocess.run([sys.executable, str(SCRIPT)], input=request,
                                           text=True, capture_output=True, env=environment, timeout=10)
            if pid_file.exists():
                pid = int(pid_file.read_text())
                try:
                    with self.assertRaises(ProcessLookupError, msg="dedicated browser must be reaped"):
                        os.kill(pid, 0)
                finally:
                    # Failure cleanup protects the test harness from orphaned fakes.
                    try:
                        os.killpg(pid, 9)
                    except ProcessLookupError:
                        pass
            requests = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            jar = (config / "cookies.txt").read_text() if (config / "cookies.txt").exists() else None
            files = sorted(item.name for item in config.iterdir()) if config.exists() else []
            return completed, requests, jar, files

    def test_full_sign_in_against_controlled_loopback_cdp_never_exports_all_cookies(self):
        completed, requests, jar, files = self.run_fake()
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertTrue(json.loads(completed.stdout)["ok"])
        self.assertEqual(completed.stderr, "")
        self.assertNotIn("offline-fake-session-sensitive", completed.stdout)
        self.assertNotIn("never-log-event-sensitive", completed.stdout)
        assert jar is not None
        self.assertIn("offline-fake-session-sensitive", jar)
        self.assertEqual(files, ["cookies.txt"])
        cookie_requests = [item for item in requests if item["method"] == "Network.getCookies"]
        self.assertTrue(cookie_requests)
        self.assertTrue(all(item["params"] == {"urls": ["https://muse.ai/"]} for item in cookie_requests))
        attachments = [item for item in requests if item["method"] == "Target.attachToTarget"]
        self.assertTrue(all(item["params"]["targetId"] == "official" for item in attachments))
        self.assertFalse(any(item["method"] in ("Network.getAllCookies", "Storage.getCookies", "Runtime.evaluate")
                             for item in requests))

    def test_timeout_cleans_browser_profile_and_does_not_save_any_jar(self):
        completed, requests, jar, files = self.run_fake("timeout")
        self.assertEqual(completed.returncode, 1)
        self.assertEqual(json.loads(completed.stdout)["error"], "timeout")
        self.assertEqual(completed.stderr, "")
        self.assertIsNone(jar)
        self.assertEqual(files, [])

    def test_sigterm_cancels_with_safe_json_and_cleans_the_browser(self):
        completed, requests, jar, files = self.run_fake("cancel")
        self.assertEqual(completed.returncode, 1)
        self.assertEqual(json.loads(completed.stdout)["error"], "cancelled")
        self.assertEqual(completed.stderr, "")
        self.assertIsNone(jar)
        self.assertEqual(files, [])

    def test_repeated_cancel_cannot_interrupt_browser_and_profile_cleanup(self):
        # The GTK disconnect path and its worker can signal cancellation twice.
        completed, requests, jar, files = self.run_fake("cancel_twice")
        self.assertEqual(completed.returncode, 1)
        self.assertEqual(json.loads(completed.stdout)["error"], "cancelled")
        self.assertEqual(completed.stderr, "")
        self.assertIsNone(jar)
        self.assertEqual(files, [])

    def test_failed_launch_does_not_leak_a_configuration_directory_fd(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as root:
            before = len(list(Path("/proc/self/fd").iterdir()))
            with patch.object(helper, "find_browser", return_value=["/missing-browser"]), \
                 patch.dict(os.environ, {"CHROME_USER_DATA_DIR": "/normal-profile", "CHROMIUM_USER_FLAGS": "--user-data-dir=/normal-profile"}, clear=True), \
                 patch.object(helper.subprocess, "Popen", side_effect=OSError("sensitive-debug-data")) as launch:
                with self.assertRaises(helper.LoginError) as error:
                    helper.login({"config_dir": str(Path(root) / "account")})
            self.assertNotIn("CHROME_USER_DATA_DIR", launch.call_args.kwargs["env"])
            self.assertNotIn("CHROMIUM_USER_FLAGS", launch.call_args.kwargs["env"])
            self.assertIn(".login-", launch.call_args.kwargs["env"]["XDG_CONFIG_HOME"])
            self.assertEqual(error.exception.code, "browser_launch_failed")
            self.assertEqual(len(list(Path("/proc/self/fd").iterdir())), before)

    def test_cdp_errors_are_redacted_and_websocket_redirects_are_refused(self):
        for mode in ("error", "redirect", "bad-handshake"):
            with self.subTest(mode=mode):
                completed, requests, jar, files = self.run_fake(mode)
                self.assertEqual(completed.returncode, 1)
                self.assertEqual(json.loads(completed.stdout)["error"], "devtools_error")
                self.assertNotIn("offline-fake-session-sensitive", completed.stdout + completed.stderr)
                self.assertEqual(completed.stderr, "")
                self.assertIsNone(jar)
                self.assertEqual(files, [])

    def test_atomic_replace_failure_keeps_previous_jar_and_removes_staging_file(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as root:
            fd = helper.open_config_dir(root)
            try:
                helper.save_cookie(fd, {"value": "old-fake", "httpOnly": True, "expires": 0})
                before = (Path(root) / "cookies.txt").read_bytes()
                with patch.object(helper.os, "replace", side_effect=OSError("fake-sensitive")):
                    with self.assertRaises(OSError):
                        helper.save_cookie(fd, {"value": "new-fake", "httpOnly": True, "expires": 0})
                self.assertEqual((Path(root) / "cookies.txt").read_bytes(), before)
                self.assertEqual(sorted(item.name for item in Path(root).iterdir()), ["cookies.txt"])
            finally:
                os.close(fd)

    def test_stdlib_websocket_handles_fragmentation_ping_and_masked_client_frames(self):
        helper = load_helper()
        left, right = socket.socketpair()
        with left, right:
            transport = helper.LoopbackWebSocket.__new__(helper.LoopbackWebSocket)
            transport.raw, transport.deadline, transport.timeout = left, time.monotonic() + 2, 1
            # Text starts, a control ping interleaves, then text continues.
            right.sendall(b'\x01\x03{"a' + b'\x89\x01p' + b'\x80\x04":1}')
            self.assertEqual(transport.recv(), '{"a":1}')
            reply = right.recv(128)
            self.assertEqual(reply[0], 0x8a)
            self.assertEqual(reply[1], 0x81)
            self.assertEqual(bytes([reply[6] ^ reply[2]]), b"p")
            transport.send("x" * 126)
            frame = right.recv(256)
            self.assertEqual(frame[:2], b"\x81\xfe")
            self.assertEqual(struct.unpack("!H", frame[2:4])[0], 126)
            self.assertEqual(bytes(value ^ frame[4 + index % 4] for index, value in enumerate(frame[8:])), b"x" * 126)

    def test_stdlib_websocket_rejects_oversize_masked_reserved_or_invalid_utf8_frames(self):
        helper = load_helper()
        invalid_frames = [b"\x81\x80", b"\xc1\x00", b"\x80\x00", b"\x09\x00",
                          b"\x81\x01\xff", b"\x82\x00", b"\x88\x00",
                          b"\x81\x7f" + struct.pack("!Q", 1048577)]
        for frame in invalid_frames:
            with self.subTest(frame=frame):
                left, right = socket.socketpair()
                with left, right:
                    transport = helper.LoopbackWebSocket.__new__(helper.LoopbackWebSocket)
                    transport.raw, transport.deadline, transport.timeout = left, time.monotonic() + 2, 1
                    right.sendall(frame)
                    with self.assertRaises(helper.LoginError) as error:
                        transport.recv()
                    self.assertEqual(error.exception.code, "devtools_error")


if __name__ == "__main__":
    unittest.main()
