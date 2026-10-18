"""Offline behavior tests: no real Muse credentials or network."""
import importlib.util
import pathlib
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "muse_account_bridge.py"


def load_bridge():
    spec = importlib.util.spec_from_file_location("account_bridge", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FakeClient:
    def status(self):
        return {"ok": True, "authenticated": True, "vm_id": "vm-1",
                "identity": {"name": "Test User", "access_token": "SECRET"},
                "vm": {"health": {"cookie": "SECRET"}}}

    def threads(self, include_archived=False):
        return [{"session_id": "thread-1", "title": "Example", "archived": False,
                 "updated_at": "2026-10-17T00:00:00Z", "raw": "SECRET"}]


class BridgeTests(unittest.TestCase):
    def test_history_keeps_latest_message_update_preserves_role_and_scope(self):
        bridge = load_bridge()
        class Client(FakeClient):
            def history(self, **kwargs):
                self.request = kwargs
                return [
                    {"message_id": "b", "seq": 7, "role": "assistant", "text": "done", "timestamp": 123, "raw": {"payload": {"status": "complete"}}},
                    {"message_id": "a", "seq": 3, "role": "user", "text": "hello"},
                    {"message_id": "b", "seq": 4, "role": "assistant", "text": "partial"},
                    {"message_id": None, "seq": 8, "role": "typing", "text": ""},
                ]
        client = Client()
        result = bridge.Adapter(client, {}).dispatch({"op": "history", "thread": " exact-id ", "limit": 20})
        self.assertEqual(client.request, {"thread": " exact-id ", "limit": 20})
        self.assertEqual([r["message_id"] for r in result], ["a", "b"])
        self.assertEqual(result[1], {"message_id": "b", "seq": 7, "role": "assistant",
            "text": "done", "timestamp": "123", "status": "complete"})

    def test_tasks_uses_only_two_read_routes_and_projects_types(self):
        from contextlib import contextmanager
        bridge = load_bridge()
        calls = []
        class Gateway:
            def call_json(self, method):
                calls.append(method)
                if method == "tasks.list":
                    return {"schedules": [{"id": "job-1", "title": "Daily", "enabled": True,
                        "timezone": "UTC", "next_run_at_utc": 123, "secret": "SECRET"}], "invalid": []}
                if method == "tasks.runs":
                    return {"runs": [{"run_id": "run-1", "job_id": "job-1", "status": "success",
                        "result_summary": "Done", "error_text": None, "scheduled_for_utc": 120, "raw": "SECRET"}]}
                raise AssertionError("nonallowlisted route")
        class Client(FakeClient):
            @contextmanager
            def _gateway(self, config):
                yield Gateway()
        result = bridge.Adapter(Client(), {}).dispatch({"op": "tasks"})
        self.assertEqual(calls, ["tasks.list", "tasks.runs"])
        self.assertEqual(result, {"schedules": [{"id": "job-1", "title": "Daily", "enabled": True,
            "timezone": "UTC", "next_run_at_utc": 123}], "runs": [{"run_id": "run-1", "job_id": "job-1",
            "status": "success", "result_summary": "Done", "error_text": None, "scheduled_for_utc": 120}]})

    def test_send_confirms_only_new_exact_user_prompt_in_same_scope(self):
        bridge = load_bridge()
        class Client(FakeClient):
            def __init__(self):
                self.sent = []
                self.reads = []
            def history(self, **kwargs):
                self.reads.append(kwargs)
                rows = [{"message_id": "old", "seq": 10, "role": "user", "text": "hello"}]
                if self.sent:
                    rows += [{"message_id": "new", "seq": 11, "role": "user", "text": "hello"}]
                return rows
            def send(self, **kwargs):
                self.sent.append(kwargs)
                return {"ok": True, "sent": True, "acknowledged": False}
        client = Client()
        result = bridge.Adapter(client, {}, confirmation_attempts=1).dispatch(
            {"op": "send", "thread": "scope-1", "message": "hello"})
        self.assertEqual(result, {"sent": True, "confirmed": True, "message_id": "new"})
        self.assertEqual(client.sent, [{"message": "hello", "thread": "scope-1", "fire_and_forget": True, "wait_seconds": 0}])
        self.assertTrue(all(r["thread"] == "scope-1" for r in client.reads))

    def test_send_failure_after_dispatch_is_uncertain_and_never_retried(self):
        bridge = load_bridge()
        class Client(FakeClient):
            sends = 0
            def history(self, **kwargs):
                if self.sends:
                    return {"ok": False, "error": {"code": "AUTH_REQUIRED", "message": "SECRET"}}
                return []
            def send(self, **kwargs):
                self.sends += 1
                return {"ok": True, "sent": True}
        client = Client()
        with self.assertRaises(bridge.BridgeFailure) as caught:
            bridge.Adapter(client, {}, confirmation_attempts=1).dispatch({"op": "send", "message": "hello"})
        self.assertEqual(caught.exception.code, "auth_required")
        self.assertTrue(caught.exception.uncertain)
        self.assertEqual(client.sends, 1)
        self.assertNotIn("SECRET", str(caught.exception))

    def test_secure_config_pins_private_cookie_file_without_writing_settings(self):
        import json, os, tempfile
        bridge = load_bridge()
        with tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR") or pathlib.Path.home()) as tmp:
            directory = pathlib.Path(tmp)
            jar = directory / "cookies.txt"
            jar.write_text("synthetic-cookie")
            jar.chmod(0o600)
            (directory / "config.json").write_text(json.dumps({"node_id": "local", "aliases": {"bad": "other"}}))
            before = (directory / "config.json").read_bytes()
            with bridge.secure_config(directory) as config:
                jar.unlink()
                jar.write_text("replaced")
                with open(config["cookies_file"]) as pinned:
                    self.assertEqual(pinned.read(), "synthetic-cookie")
                self.assertEqual(config["node_id"], "local")
                self.assertNotIn("aliases", config)
            self.assertEqual((directory / "config.json").read_bytes(), before)

    def test_invalid_requests_never_load_community_client(self):
        bridge = load_bridge()
        calls = []
        def factory(config):
            calls.append(config)
            raise AssertionError("network client must not be created")
        for bad in [
            {"op": "raw", "method": "node.register"},
            {"op": "history", "limit": 0},
            {"op": "history", "limit": True},
            {"op": "history", "limit": 201},
            {"op": "history", "limit": 1, "thread": "  "},
            {"op": "send", "message": "  "},
            {"op": "connect", "raw": True},
            {"op": "tasks", "message": "unneeded"},
        ]:
            bad["config_dir"] = "/nonexistent"
            with self.subTest(bad=bad):
                result = bridge.handle_request(bad, client_factory=factory)
                self.assertEqual(result, {"ok": False, "error": {"code": "invalid_request", "uncertain": False}})
        self.assertEqual(calls, [])

    def test_sidecar_protocol_rejects_malformed_or_oversized_stdin_safely(self):
        import json, subprocess, sys
        for data in [b'{SECRET', b'x' * (1024 * 1024 + 1)]:
            with self.subTest(size=len(data)):
                result = subprocess.run([sys.executable, "-I", str(SCRIPT)], input=data,
                    capture_output=True, timeout=5)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stderr, b"")
                self.assertEqual(json.loads(result.stdout), {"ok": False,
                    "error": {"code": "invalid_request", "uncertain": False}})
                self.assertNotIn(b'SECRET', result.stdout)

    def test_task_error_text_never_exposes_credential_shaped_text(self):
        from contextlib import contextmanager
        bridge = load_bridge()
        class Gateway:
            def call_json(self, method):
                if method == "tasks.list":
                    return {"schedules": []}
                return {"runs": [{"run_id": "r", "job_id": "j", "status": "failed",
                    "error_text": 'transport Cookie: hatch_sess=SECRET; token=SECRET Bearer SECRET',
                    "result_summary": 'access_token=SECRET'}]}
        class Client(FakeClient):
            @contextmanager
            def _gateway(self, config):
                yield Gateway()
        result = bridge.Adapter(Client(), {}).dispatch({"op": "tasks"})
        self.assertNotIn("SECRET", str(result))
        self.assertIn("REDACTED", result["runs"][0]["error_text"])

    def test_malformed_upstream_history_is_a_safe_protocol_error(self):
        bridge = load_bridge()
        class Client(FakeClient):
            def history(self, **kwargs):
                return ["SECRET"]
        with self.assertRaises(bridge.BridgeFailure) as caught:
            bridge.Adapter(Client(), {}).dispatch({"op": "history", "limit": 20})
        self.assertEqual(caught.exception.code, "protocol_error")
        self.assertNotIn("SECRET", str(caught.exception))

    def test_connect_requires_an_authenticated_vm(self):
        bridge = load_bridge()
        cases = [({"ok": True, "authenticated": False, "vm_id": "v", "identity": {}}, "auth_required"),
                 ({"ok": True, "authenticated": True, "vm_id": "", "identity": {}}, "protocol_error")]
        for status, code in cases:
            class Client(FakeClient):
                def status(self):
                    return status
            with self.subTest(code=code):
                with self.assertRaises(bridge.BridgeFailure) as caught:
                    bridge.Adapter(Client(), {}).dispatch({"op": "connect"})
                self.assertEqual(caught.exception.code, code)

    def test_permissions_and_symlinks_are_rejected_before_client_creation(self):
        import os, tempfile
        bridge = load_bridge()
        for case in ("directory_mode", "cookie_mode", "cookie_symlink", "config_symlink", "parent_symlink", "config_mode"):
            with self.subTest(case=case), tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR") or pathlib.Path.home()) as tmp:
                root = pathlib.Path(tmp)
                directory = root / "private"
                directory.mkdir(mode=0o700)
                jar = directory / "cookies.txt"
                jar.write_text("synthetic-cookie")
                jar.chmod(0o600)
                settings = directory / "config.json"
                settings.write_text("{}")
                settings.chmod(0o644)
                chosen = directory
                if case == "directory_mode": directory.chmod(0o755)
                if case == "cookie_mode": jar.chmod(0o644)
                if case == "config_mode": settings.chmod(0o666)
                if case == "cookie_symlink":
                    jar.rename(directory / "real-cookie")
                    jar.symlink_to(directory / "real-cookie")
                if case == "config_symlink":
                    settings.rename(directory / "real-config")
                    settings.symlink_to(directory / "real-config")
                if case == "parent_symlink":
                    chosen = root / "link"
                    chosen.symlink_to(directory, target_is_directory=True)
                result = bridge.handle_request({"op": "connect", "config_dir": str(chosen)},
                    client_factory=lambda _: self.fail("must not create network client"))
                self.assertEqual(result["error"]["code"], "insecure_config")

    def test_missing_cookie_is_auth_required_and_does_not_create_files(self):
        import os, tempfile
        bridge = load_bridge()
        with tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR") or pathlib.Path.home()) as tmp:
            result = bridge.handle_request({"op": "connect", "config_dir": tmp},
                client_factory=lambda _: self.fail("must not create client"))
            self.assertEqual(result["error"]["code"], "auth_required")
            self.assertEqual(list(pathlib.Path(tmp).iterdir()), [])

    def test_send_unconfirmed_for_old_prompt_assistant_echo_or_changed_text(self):
        bridge = load_bridge()
        for candidate in [
            {"message_id": "old", "seq": 10, "role": "user", "text": "hello"},
            {"message_id": "new", "seq": 11, "role": "assistant", "text": "hello"},
            {"message_id": "new", "seq": 11, "role": "user", "text": "hello!"},
            {"message_id": "old", "seq": 11, "role": "user", "text": "hello"},
        ]:
            class Client(FakeClient):
                sends = 0
                def history(self, **kwargs):
                    if self.sends: return [candidate]
                    return [{"message_id": "old", "seq": 10, "role": "user", "text": "hello"}]
                def send(self, **kwargs):
                    self.sends += 1
                    return {"ok": True, "sent": True, "acknowledged": True}
            client = Client()
            result = bridge.Adapter(client, {}, confirmation_attempts=1).dispatch({"op": "send", "message": "hello"})
            self.assertEqual(result, {"sent": True, "confirmed": False, "message_id": None})
            self.assertEqual(client.sends, 1)

    def test_send_does_not_dispatch_if_baseline_fails(self):
        bridge = load_bridge()
        class Client(FakeClient):
            def history(self, **kwargs):
                return {"ok": False, "error": {"code": "VM_UNAVAILABLE", "message": "SECRET"}}
            def send(self, **kwargs):
                raise AssertionError("must not dispatch")
        with self.assertRaises(bridge.BridgeFailure) as caught:
            bridge.Adapter(Client(), {}).dispatch({"op": "send", "message": "hello"})
        self.assertEqual(caught.exception.code, "vm_unavailable")
        self.assertFalse(caught.exception.uncertain)

    def test_exceptions_and_structured_auth_vm_timeout_errors_are_safe(self):
        bridge = load_bridge()
        for code in ("AUTH_REQUIRED", "VM_UNAVAILABLE", "TIMEOUT"):
            class Client(FakeClient):
                def status(self):
                    return {"ok": False, "error": {"code": code, "message": "SECRET"}}
            with self.assertRaises(bridge.BridgeFailure) as caught:
                bridge.Adapter(Client(), {}).dispatch({"op": "connect"})
            self.assertEqual(caught.exception.code, code.lower())
            self.assertNotIn("SECRET", str(caught.exception))
        error = bridge.classify_exception(RuntimeError("Bearer SECRET; cookie=SECRET"))
        self.assertEqual(str(error), "bridge_failure")

    def test_http_only_netscape_cookie_is_compatible_with_external_loader(self):
        import os, tempfile
        bridge = load_bridge()
        with tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR") or pathlib.Path.home()) as tmp:
            jar = pathlib.Path(tmp) / "cookies.txt"
            source = b"# Netscape HTTP Cookie File\n#HttpOnly_muse.ai\tFALSE\t/\tTRUE\t0\thatch_sess\tsynthetic-only\n"
            jar.write_bytes(source)
            jar.chmod(0o600)
            with bridge.secure_config(pathlib.Path(tmp)) as config:
                with open(config["cookies_file"], "rb") as pinned:
                    data = pinned.read()
                self.assertIn(b"muse.ai\tFALSE", data)
                self.assertNotIn(b"#HttpOnly_", data)
            self.assertEqual(jar.read_bytes(), source)

    def test_send_requires_a_positive_dispatch_receipt_before_confirming(self):
        bridge = load_bridge()
        class Client(FakeClient):
            sends = 0
            def history(self, **kwargs):
                return [] if not self.sends else [{"message_id": "n", "seq": 1, "role": "user", "text": "hello"}]
            def send(self, **kwargs):
                self.sends += 1
                return {"ok": True, "sent": False}
        client = Client()
        with self.assertRaises(bridge.BridgeFailure) as caught:
            bridge.Adapter(client, {}, confirmation_attempts=1).dispatch({"op": "send", "message": "hello"})
        self.assertEqual(caught.exception.code, "protocol_error")
        self.assertTrue(caught.exception.uncertain)
        self.assertEqual(client.sends, 1)

    def test_real_client_waits_for_gateway_response_before_closing_dispatch(self):
        # Regression: upstream fire-and-forget closes Noise immediately after
        # writing the request; the live prompt never reached shared history.
        import types
        from unittest.mock import patch
        from contextlib import contextmanager
        bridge = load_bridge()
        events = []
        class Gateway:
            def request(self, method, **kwargs):
                events.append((method, kwargs))
                return b'{"ok":true}'
        class BaseClient:
            def __init__(self, config_loader):
                self.config_loader = config_loader
            @contextmanager
            def _gateway(self, config):
                yield Gateway()
                events.append('closed')
            def send(self, **kwargs):
                raise AssertionError('must not use unacknowledged fire-and-forget')
        module = types.ModuleType('muse_mcp.client')
        setattr(module, 'MuseClient', BaseClient)
        with patch.dict('sys.modules', {'muse_mcp.client': module}):
            client = bridge.community_client({'node_id': 'linux-test'})
            result = client.send(message='hello', thread='selected-thread', fire_and_forget=True, wait_seconds=0)
        self.assertTrue(result['sent'])
        self.assertEqual(events[-1], 'closed')
        self.assertEqual(events[0][0], 'chat.stream')
        self.assertEqual(events[0][1]['body']['session_id'], 'selected-thread')
        self.assertEqual(events[0][1]['body']['items'], [{'type': 'text', 'text': 'hello'}])

    def test_connect_projects_only_typed_identity_and_threads(self):
        bridge = load_bridge()
        response = bridge.Adapter(FakeClient(), {}).dispatch({"op": "connect"})
        self.assertEqual(response, {"identity_name": "Test User", "vm_id": "vm-1",
            "threads": [{"session_id": "thread-1", "title": "Example",
                         "archived": False, "updated_at": "2026-10-17T00:00:00Z"}]})
        self.assertNotIn("SECRET", str(response))


if __name__ == "__main__":
    unittest.main()
