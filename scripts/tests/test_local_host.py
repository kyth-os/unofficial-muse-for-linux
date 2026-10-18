"""Credential-free local-host transport regression tests."""
import importlib.util
import json
from pathlib import Path
import struct
import os
import threading
import time
from contextlib import contextmanager
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "muse_local_host.py"


def module():
    assert SCRIPT.is_file(), "local host sidecar is not implemented"
    spec = importlib.util.spec_from_file_location("local_host_test", SCRIPT)
    assert spec is not None and spec.loader is not None
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


class DecoderTests(unittest.TestCase):
    def test_decoder_rejects_per_tick_message_flood(self):
        host = module()
        with self.assertRaises(host.Failure):
            host.Decoder().feed(host.encode({}) * 65)

    def test_fragmented_and_multiple_length_prefixes(self):
        host = module()
        decoder = host.Decoder()
        wire = host.encode({"id": "a"}) + host.encode({"id": "b"})
        self.assertEqual(decoder.feed(wire[:2]), [])
        self.assertEqual(decoder.feed(wire[2:7]), [])
        self.assertEqual(decoder.feed(wire[7:]), [{"id": "a"}, {"id": "b"}])

    def test_rejects_oversized_malformed_duplicate_and_nonobject(self):
        host = module()
        invalid = [struct.pack("<I", host.MAX_MESSAGE + 1),
                   struct.pack("<I", 1) + b"{",
                   struct.pack("<I", 13) + b'{"a":1,"a":2}',
                   host.encode([]), struct.pack("<I", 9) + b'{"a":NaN}']
        for wire in invalid:
            with self.subTest(wire=wire[:20]), self.assertRaises(host.Failure):
                host.Decoder().feed(wire)
        with self.assertRaises(host.Failure):
            host.encode({"data": "x" * host.MAX_MESSAGE})
        with self.assertRaises(host.Failure):
            host.Decoder().feed(b"\x00" * (host.MAX_MESSAGE + 5))


START = {"op": "start", "config_dir": "/nonexistent-safe-test", "node_id": "test-node",
         "display_name": "Test Linux", "commands_v2": {"safe.read": {"description": "read"}}}


class WireTests(unittest.TestCase):
    def setUp(self):
        try:
            import muse
        except ImportError:
            self.skipTest("selected community runtime required for protobuf tests")
        self.proto = muse
        self.host = module()
        self.events = []
        self.sent = []
        class Gateway:
            stream = 7
            def _send_envelope(inner, service_id, raw):
                frame = muse.ServiceFrame()
                frame.ParseFromString(raw)
                self.sent.append((service_id, frame))
        self.gateway = Gateway()

    def response(self, status=200, body=b"", ended=False):
        frame = self.proto.ServiceFrame(stream_id=7)
        frame.response.status = status
        frame.response.body = body
        frame.response.end_body = ended
        return frame

    def chunk(self, message):
        frame = self.proto.ServiceFrame(stream_id=7)
        frame.body_chunk.data = self.host.encode(message)
        return frame

    def test_rejection_reset_eof_and_unpair_are_terminal(self):
        for mode, code in [("401", "auth_required"), ("403", "link_forbidden"),
                           ("500", "link_rejected"), ("ack", "link_rejected"),
                           ("reset", "stream_reset"), ("eof", "stream_closed"),
                           ("link.unpaired", "unpaired"), ("node.unpaired", "unpaired")]:
            with self.subTest(mode=mode):
                link = self.host.Link(self.gateway, self.proto, START, self.events.append)
                link.stream_id = 7
                frame = self.response(int(mode)) if mode.isdigit() else self.response()
                if mode == "ack":
                    frame = self.chunk({"id": link.register_id, "error": {"secret": "private"}})
                elif mode == "reset":
                    frame = self.proto.ServiceFrame(stream_id=7)
                    frame.reset.code = 1
                elif mode == "eof":
                    frame.response.end_body = True
                elif mode.endswith("unpaired"):
                    frame = self.chunk({"event": mode})
                with self.assertRaises(self.host.Failure) as caught:
                    link.frame(frame)
                self.assertEqual(caught.exception.code, code)
                with self.assertRaises(self.host.Failure):
                    link.frame(self.response())
        self.assertEqual(self.events, [])

    def registered(self, clock=lambda: 1.0):
        self.gateway.stream = 7
        link = self.host.Link(self.gateway, self.proto, START, self.events.append, clock=clock)
        link.frame(self.response())
        link.frame(self.chunk({"id": link.register_id}))
        return link

    def invoke(self, id="i", command="safe.read", timeout=1000):
        return self.chunk({"method": "link.invoke", "id": id, "command": command,
                           "params": {"path": "test"}, "timeout_ms": timeout})

    def test_invocation_result_is_once_only_bounded_and_uses_payload(self):
        link = self.registered()
        link.frame(self.invoke())
        self.assertEqual(self.events[-1], {"type": "invoke", "id": "i", "command": "safe.read",
                                         "params": {"path": "test"}, "timeout_ms": 1000})
        link.result({"op": "result", "id": "i", "ok": True, "output": {"text": "done"}})
        result = self.host.Decoder().feed(self.sent[-1][1].body_chunk.data)[0]
        self.assertEqual(result, {"method": "link.result", "id": "i", "ok": True,
                                  "payload": {"text": "done"}})
        for id in ["i", "unknown"]:
            with self.assertRaises(self.host.Failure):
                link.result({"op": "result", "id": id, "ok": True, "output": {}})
        with self.assertRaises(self.host.Failure):
            link.frame(self.invoke())

    def test_unknown_expired_duplicate_and_over_budget_invokes_fail_closed(self):
        now = [1.0]
        link = self.registered(clock=lambda: now[0])
        link.frame(self.invoke("expires", timeout=1))
        now[0] = 2.0
        link.expire()
        self.assertFalse(link.pending)
        with self.assertRaises(self.host.Failure):
            link.result({"op": "result", "id": "expires", "ok": True, "output": {}})
        for invalid in [self.invoke(command="shell"), self.invoke(timeout=0),
                        self.invoke(timeout=True)]:
            with self.assertRaises(self.host.Failure):
                self.registered().frame(invalid)
        link = self.registered()
        for i in range(self.host.MAX_PENDING):
            link.frame(self.invoke(str(i)))
        with self.assertRaises(self.host.Failure):
            link.frame(self.invoke("overflow"))

    def test_result_escape_inflation_becomes_safe_error_not_private_output(self):
        link = self.registered()
        link.frame(self.invoke())
        link.result({"op": "result", "id": "i", "ok": True,
                     "output": "\u2603" * (self.host.MAX_MESSAGE // 5)})
        result = self.host.Decoder().feed(self.sent[-1][1].body_chunk.data)[0]
        self.assertFalse(result["ok"])
        self.assertEqual(result["error"], "result_too_large")
        self.assertNotIn("payload", result)

    def test_unknown_wire_methods_and_pre_registration_invokes_are_rejected(self):
        for message in [{"method": "raw.command"},
                        {"method": "link.invoke", "id": "x", "command": "safe.read", "params": {}}]:
            link = self.host.Link(self.gateway, self.proto, START, self.events.append)
            link.stream_id = 7
            with self.assertRaises(self.host.Failure):
                link.frame(self.chunk(message))

    def test_register_only_after_http_success_and_matching_ack(self):
        link = self.host.Link(self.gateway, self.proto, START, self.events.append)
        link.open()
        sid, request = self.sent[0]
        self.assertEqual(sid, 0)
        self.assertEqual(request.stream_id, 7)
        self.assertEqual(request.request.verb, "POST")
        self.assertEqual(request.request.path, "/link-control")
        self.assertFalse(request.request.end_body)
        headers = {h.key: h.value for h in request.request.headers}
        # SDK control-stream open is headerless; do not invent an app identity.
        self.assertEqual(headers, {})
        register = self.host.Decoder().feed(self.sent[1][1].body_chunk.data)[0]
        self.assertEqual(register["method"], "link.register")
        self.assertEqual(register["params"]["commands_v2"], START["commands_v2"])
        self.assertEqual(register["params"]["platform"], "linux")
        self.assertEqual(register["params"]["device_family"], "homehub")
        link.frame(self.chunk({"id": register["id"]}))
        self.assertEqual(self.events, [])
        link.frame(self.response())
        self.assertEqual(self.events, [{"type": "state", "state": "registered"}])
        self.assertEqual(self.gateway.stream, 8)


class LifecycleTests(unittest.TestCase):
    def test_start_validation_rejects_unknown_controls_and_unbounded_manifests(self):
        host = module()
        host.validate_start(START)
        for request in [[], {**START, "op": "raw"}, {**START, "extra": 1},
                        {**START, "node_id": ""}, {**START, "config_dir": "relative"},
                        {**START, "commands_v2": []}, {**START, "commands_v2": {"": {}}},
                        {**START, "commands_v2": {"a": "not a manifest"}},
                        {**START, "commands_v2": {str(i): {} for i in range(65)}}]:
            with self.subTest(request=str(request)[:100]), self.assertRaises(host.Failure):
                host.validate_start(request)

    def test_gateway_cleanup_is_once_only_even_when_context_and_stopper_close(self):
        host = module()
        class Socket:
            def recv_fragment(self):
                raise EOFError()
        class Gateway:
            ws = Socket()
            count = 0
            def close(self):
                self.count += 1
        gateway = Gateway()
        host.install_bounded_receiver(gateway)
        gateway.close()
        gateway.close()
        self.assertEqual(gateway.count, 1)

    def test_bounded_websocket_fragment_assembly_does_not_assume_idle_is_auth(self):
        host = module()
        from types import SimpleNamespace
        try:
            from curl_cffi import CurlWsFlag
        except ImportError:
            self.skipTest("selected runtime required")
        class Socket:
            closed = False
            def __init__(self, fragments):
                self.fragments = iter(fragments)
            def recv_fragment(self):
                return next(self.fragments)
        frame = lambda left, flags: SimpleNamespace(bytesleft=left, flags=flags)
        websocket = Socket([(b'a',frame(1,CurlWsFlag.BINARY)),(b'b',frame(0,CurlWsFlag.BINARY))])
        self.assertEqual(host.bounded_recv(websocket)[0], b'ab')
        for frames in [[(b'',frame(host.MAX_NOISE+1,CurlWsFlag.BINARY))],
                       [(b'x'*(host.MAX_NOISE+1),frame(0,CurlWsFlag.BINARY))],
                       [(b'text',frame(0,CurlWsFlag.TEXT))]]:
            with self.assertRaises(host.Failure):
                host.bounded_recv(Socket(frames))
        self.assertEqual(host.safe_code(TimeoutError('private')), 'transport_timeout')
        self.assertEqual(host.safe_code(Exception('private')), 'transport_error')

    def test_cli_isolated_import_invalid_request_and_repeated_signals(self):
        import subprocess
        import sys
        import signal
        invalid = subprocess.run([sys.executable, "-I", str(SCRIPT)], input=b'{"op":"raw","secret":"private"}\n',
                                 capture_output=True, timeout=2)
        self.assertEqual(json.loads(invalid.stdout), {"type": "state", "state": "error", "code": "invalid_request"})
        self.assertEqual(invalid.stderr, b"")
        eof = subprocess.run([sys.executable, "-I", str(SCRIPT)], input=b'',
                             capture_output=True, timeout=2)
        self.assertEqual(json.loads(eof.stdout), {"type":"state","state":"closed","code":"eof"})
        process = subprocess.Popen([sys.executable, "-I", str(SCRIPT)], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(0.1)
        process.send_signal(signal.SIGTERM)
        process.send_signal(signal.SIGTERM)
        output, stderr = process.communicate(timeout=2)
        self.assertEqual(process.returncode, 0)
        self.assertEqual(json.loads(output)["code"], "stopped")
        self.assertEqual(stderr, b"")

    def test_default_connector_reuses_bridge_helpers_without_loading_saved_credentials(self):
        try:
            import muse
        except ImportError:
            self.skipTest("selected runtime required")
        host = module()
        from unittest.mock import patch
        calls = []
        fake_gateway = object()
        @contextmanager
        def secure(directory):
            calls.append(("secure", directory))
            yield {"node_id": "old-node", "cookies_file": "memfd-test"}
        class Client:
            @contextmanager
            def _gateway(self, config):
                calls.append(("gateway", config["node_id"]))
                yield fake_gateway
        class Bridge:
            secure_config = staticmethod(secure)
            @staticmethod
            def community_client(config):
                calls.append(("client", config["node_id"]))
                return Client()
        with patch.object(host, "bridge_module", return_value=Bridge), \
                patch.object(host, "install_bounded_receiver") as installed:
            with host.community_connector(START) as (gateway, proto):
                self.assertIs(gateway, fake_gateway)
                self.assertTrue(hasattr(proto, "ServiceFrame"))
            installed.assert_called_once_with(fake_gateway)
        self.assertEqual(calls, [("secure", START["config_dir"]), ("client", "test-node"),
                                 ("gateway", "test-node")])
        loaded = host.bridge_module()
        self.assertTrue(callable(loaded.secure_config))
        self.assertTrue(callable(loaded.community_client))

    def test_run_registration_invoke_result_and_idle_shutdown(self):
        host = module()
        try:
            import muse
        except ImportError:
            self.skipTest("selected runtime required")
        import queue
        inbound = queue.Queue()
        closed = threading.Event()
        result_sent = threading.Event()
        read_fd, write_fd = os.pipe()
        events, wire_results = [], []
        class Gateway:
            stream = 1
            def _send_envelope(self, service, raw):
                assert service == 0
                frame = muse.ServiceFrame()
                frame.ParseFromString(raw)
                if frame.WhichOneof('kind') == 'request':
                    response = muse.ServiceFrame(stream_id=1)
                    response.response.status = 200
                    inbound.put(response)
                else:
                    message = host.Decoder().feed(frame.body_chunk.data)[0]
                    if message.get('method') == 'link.register':
                        response = muse.ServiceFrame(stream_id=1)
                        response.body_chunk.data = host.encode({'id':message['id']}) + host.encode({
                            'method':'link.invoke','id':'run','command':'safe.read','params':{},'timeout_ms':1000})
                        inbound.put(response)
                    elif message.get('method') == 'link.result':
                        wire_results.append(message)
                        result_sent.set()
            def _read_frame(self):
                value = inbound.get(timeout=2)
                if value is None:
                    raise EOFError()
                return value
            def close(self):
                closed.set()
                inbound.put(None)
        @contextmanager
        def connector(start):
            try:
                yield Gateway(), muse
            finally:
                closed.set()
        def emit(event):
            events.append(event)
            if event['type'] == 'invoke':
                os.write(write_fd,b'{"op":"result","id":"run","ok":true,"output":{"x":1}}\n')
        thread = threading.Thread(target=host.run,args=(START,read_fd,emit,threading.Event()),kwargs={'connector':connector})
        thread.start()
        self.assertTrue(result_sent.wait(2))
        os.write(write_fd,b'{"op":"stop"}\n')
        thread.join(2)
        os.close(read_fd)
        os.close(write_fd)
        self.assertFalse(thread.is_alive())
        self.assertTrue(closed.is_set())
        self.assertEqual([e.get('state',e['type']) for e in events], ['connecting','registered','invoke','closed'])
        self.assertEqual(wire_results,[{'method':'link.result','id':'run','ok':True,'payload':{'x':1}}])

    def test_idle_socket_stop_eof_and_unknown_control_cleanup(self):
        host = module()
        try:
            import muse
        except ImportError:
            self.skipTest("selected runtime required")
        for operation, expected in [(b'{"op":"stop"}\n', "stopped"), (b'', "eof"),
                                    (b'{"op":"raw"}\n', "invalid_request")]:
            with self.subTest(operation=operation):
                read_fd, write_fd = os.pipe()
                closed = threading.Event()
                ready = threading.Event()
                events = []
                class Gateway:
                    stream = 1
                    def _send_envelope(self, *args):
                        ready.set()
                    def _read_frame(self):
                        closed.wait(5)
                        raise EOFError()
                    def close(self):
                        closed.set()
                @contextmanager
                def connector(start):
                    try:
                        yield Gateway(), muse
                    finally:
                        closed.set()
                stop = threading.Event()
                thread = threading.Thread(target=host.run,
                    args=(START, read_fd, events.append, stop), kwargs={"connector": connector})
                thread.start()
                self.assertTrue(ready.wait(1))
                if operation:
                    os.write(write_fd, operation)
                else:
                    os.close(write_fd)
                    write_fd = None
                thread.join(1)
                self.assertFalse(thread.is_alive())
                self.assertTrue(closed.wait(1))
                self.assertEqual(events[0], {"type": "state", "state": "connecting"})
                self.assertEqual(events[-1]["code"], expected)
                self.assertNotIn("registered", [e.get("state") for e in events])
                os.close(read_fd)
                if write_fd is not None:
                    os.close(write_fd)


if __name__ == "__main__":
    unittest.main()
