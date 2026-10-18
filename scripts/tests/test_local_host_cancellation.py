"""Cancellation must discard a control frame already removed from its queue."""
import os
import queue
import threading
import unittest
from contextlib import contextmanager
from unittest.mock import patch

from test_local_host import START, module


class QueuedSendCancellationTests(unittest.TestCase):
    def test_stop_discards_dequeued_registration_and_private_result(self):
        import muse

        for target in ("request", "link.register", "link.result"):
            with self.subTest(target=target):
                host = module()
                stop = threading.Event()
                dequeued = threading.Event()
                release = threading.Event()
                cleanup_started = threading.Event()
                finished = threading.Event()
                real_queue = queue.Queue
                inbound = real_queue()
                sent_after_stop = []
                events = []
                read_fd, write_fd = os.pipe()
                queue_count = 0

                def frame_kind(raw):
                    frame = muse.ServiceFrame()
                    frame.ParseFromString(raw)
                    if frame.WhichOneof("kind") == "request":
                        return "request", frame
                    messages = host.Decoder().feed(frame.body_chunk.data)
                    return messages[0].get("method"), frame

                class GatedQueue(real_queue):
                    def __init__(self, *args, **kwargs):
                        nonlocal queue_count
                        super().__init__(*args, **kwargs)
                        queue_count += 1
                        self.outgoing = queue_count == 2

                    def get(self, *args, **kwargs):
                        item = super().get(*args, **kwargs)
                        if self.outgoing and frame_kind(item[1])[0] == target:
                            dequeued.set()
                            if not release.wait(3):
                                raise RuntimeError("test scheduling gate timed out")
                        return item

                class Gateway:
                    stream = 1

                    def _send_envelope(self, service, raw):
                        kind, frame = frame_kind(raw)
                        if stop.is_set():
                            sent_after_stop.append(kind)
                        response = muse.ServiceFrame(stream_id=1)
                        if kind == "request":
                            response.response.status = 200
                            inbound.put(response)
                        elif kind == "link.register":
                            message = host.Decoder().feed(frame.body_chunk.data)[0]
                            response.body_chunk.data = host.encode({
                                "id": message["id"], "ok": True,
                            }) + host.encode({
                                "method": "link.invoke", "id": "cancel-private-result",
                                "command": "safe.read", "params": {}, "timeout_ms": 10000,
                            })
                            inbound.put(response)

                    def _read_frame(self):
                        item = inbound.get(timeout=3)
                        if item is None:
                            raise EOFError()
                        return item

                    def close(self):
                        cleanup_started.set()
                        inbound.put(None)
                        # Real dependency close can block; it is not a send barrier.
                        release.wait(3)

                @contextmanager
                def connector(start):
                    try:
                        yield Gateway(), muse
                    finally:
                        finished.set()

                def emit(event):
                    events.append(event)
                    if event["type"] == "invoke":
                        os.write(write_fd, b'{"op":"result","id":"cancel-private-result",'
                                 b'"ok":true,"output":{"content":"private scratch fixture"}}\n')

                thread = threading.Thread(target=host.run,
                    args=(START, read_fd, emit, stop), kwargs={"connector": connector})
                try:
                    with patch.object(host.queue, "Queue", GatedQueue):
                        thread.start()
                        self.assertTrue(dequeued.wait(3), "outgoing frame was not gated")
                        os.write(write_fd, b'{"op":"stop"}\n')
                        self.assertTrue(cleanup_started.wait(3), "stop cleanup did not begin")
                        self.assertTrue(stop.is_set())
                        release.set()
                        thread.join(3)
                        self.assertFalse(thread.is_alive())
                        self.assertTrue(finished.wait(3))
                    self.assertEqual(sent_after_stop, [], "dequeued frame sent after stop")
                    self.assertEqual(events[-1]["code"], "stopped")
                finally:
                    stop.set()
                    release.set()
                    inbound.put(None)
                    thread.join(3)
                    os.close(read_fd)
                    os.close(write_fd)
