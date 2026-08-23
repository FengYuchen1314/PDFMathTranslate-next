import asyncio
import queue
import threading
from pathlib import Path

import pytest
from pdf2zh_next import high_level


class FakeConnection:
    def __init__(self, messages=None):
        self.messages = queue.Queue()
        for message in messages or []:
            self.messages.put(message)
        self.closed = False

    def recv(self):
        message = self.messages.get(timeout=2)
        if isinstance(message, BaseException):
            raise message
        return message

    def send(self, message):
        if self.closed:
            raise OSError("closed")
        self.messages.put(message)

    def close(self):
        self.closed = True
        # Release a fake receiver that is waiting for cancellation.
        self.messages.put(EOFError())


class FakeLogQueue:
    def __init__(self):
        self.messages = queue.Queue()
        self.messages.put(None)
        self.closed = False
        self.cancelled_join = False

    def get(self, timeout=None):
        return self.messages.get(timeout=timeout)

    def cancel_join_thread(self):
        self.cancelled_join = True

    def close(self):
        self.closed = True

    def join_thread(self):
        raise AssertionError("cleanup must never perform an unbounded queue join")


class FakeProcess:
    def __init__(self, *, start_error=None):
        self.pid = 123456
        self.exitcode = None
        self.alive = False
        self.closed = False
        self.start_error = start_error
        self.join_calls = []

    def start(self):
        if self.start_error is not None:
            raise self.start_error
        self.alive = True

    def is_alive(self):
        return self.alive

    def join(self, timeout=None):
        self.join_calls.append(timeout)
        self.alive = False
        self.exitcode = 0

    def terminate(self):
        self.alive = False
        self.exitcode = -15

    def kill(self):
        self.alive = False
        self.exitcode = -9

    def close(self):
        self.closed = True


class FakeValue:
    def __init__(self, value):
        self.value = value


class FakeContext:
    def __init__(self, progress_messages, *, start_error=None, group_value=0):
        self.progress_recv = FakeConnection(progress_messages)
        self.progress_send = FakeConnection()
        self.cancel_recv = FakeConnection()
        self.cancel_send = FakeConnection()
        self.log_queue = FakeLogQueue()
        self.process = FakeProcess(start_error=start_error)
        self.group_value = group_value
        self.pipe_count = 0

    def Pipe(self, duplex=False):  # noqa: N802 - mirrors multiprocessing context
        assert duplex is False
        self.pipe_count += 1
        if self.pipe_count == 1:
            return self.progress_recv, self.progress_send
        return self.cancel_recv, self.cancel_send

    def Queue(self):  # noqa: N802 - mirrors multiprocessing context
        return self.log_queue

    def Value(self, typecode, value):  # noqa: N802 - mirrors multiprocessing context
        assert typecode == "q"
        assert value == 0
        return FakeValue(self.group_value)

    def Process(self, target, args):  # noqa: N802 - mirrors multiprocessing context
        assert target is high_level._translate_wrapper
        assert len(args) == 6
        return self.process


def test_finish_is_published_only_after_worker_and_ipc_cleanup(monkeypatch):
    finish = {"type": "finish", "translate_result": object()}
    context = FakeContext([{"type": "progress_start"}, finish, None])
    monkeypatch.setattr(high_level.multiprocessing, "get_context", lambda _: context)

    async def collect():
        observed = []
        async for event in high_level._translate_in_subprocess(None, Path("input.pdf")):
            if event["type"] == "finish":
                assert context.process.closed is True
                assert context.progress_recv.closed is True
                assert context.log_queue.closed is True
            observed.append(event)
        return observed

    assert asyncio.run(collect()) == [{"type": "progress_start"}, finish]
    assert context.log_queue.cancelled_join is True


def test_aclose_cancels_worker_without_reporting_a_crash(monkeypatch):
    context = FakeContext([{"type": "progress_update"}])
    monkeypatch.setattr(high_level.multiprocessing, "get_context", lambda _: context)

    async def close_after_progress():
        stream = high_level._translate_in_subprocess(None, Path("input.pdf"))
        assert (await anext(stream))["type"] == "progress_update"
        await stream.aclose()

    asyncio.run(close_after_progress())

    assert context.cancel_send.messages.get_nowait() is True
    assert context.process.closed is True
    assert context.log_queue.closed is True


def test_clean_exit_without_finish_is_an_ipc_error(monkeypatch):
    context = FakeContext([None])
    monkeypatch.setattr(high_level.multiprocessing, "get_context", lambda _: context)

    async def consume():
        async for _event in high_level._translate_in_subprocess(
            None, Path("input.pdf")
        ):
            pass

    with pytest.raises(high_level.IPCError, match="without a finish event"):
        asyncio.run(consume())
    assert context.process.closed is True


def test_failed_process_group_isolation_is_unrecoverable(monkeypatch):
    finish = {"type": "finish", "translate_result": object()}
    context = FakeContext([finish, None], group_value=-1)
    monkeypatch.setattr(high_level.multiprocessing, "get_context", lambda _: context)

    async def consume():
        async for _event in high_level._translate_in_subprocess(
            None, Path("input.pdf")
        ):
            pass

    with pytest.raises(
        high_level.UnrecoverableCleanupError,
        match="isolated process group",
    ):
        asyncio.run(consume())
    assert context.process.closed is True


def test_process_start_failure_closes_every_allocated_resource(monkeypatch):
    context = FakeContext([], start_error=OSError("spawn failed"))
    monkeypatch.setattr(high_level.multiprocessing, "get_context", lambda _: context)

    async def start():
        await anext(high_level._translate_in_subprocess(None, Path("input.pdf")))

    with pytest.raises(OSError, match="spawn failed"):
        asyncio.run(start())

    assert context.progress_recv.closed is True
    assert context.progress_send.closed is True
    assert context.cancel_recv.closed is True
    assert context.cancel_send.closed is True
    assert context.log_queue.closed is True
    assert context.process.closed is True
    assert not any(
        thread.name in {"pdf2zh-progress-listener", "pdf2zh-log-listener"}
        for thread in threading.enumerate()
    )
