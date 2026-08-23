import io
import json
import os
from types import SimpleNamespace

from pdf2zh_next import worker_supervisor


def test_supervisor_requires_private_worker_token(monkeypatch):
    monkeypatch.setattr(worker_supervisor, "WORKER_TOKEN", "")

    assert worker_supervisor.main() == 64


def test_supervisor_requires_public_one_shot_mode(monkeypatch):
    monkeypatch.setattr(worker_supervisor, "WORKER_TOKEN", "test-token")
    monkeypatch.setattr(worker_supervisor, "PUBLIC_WORKER_MODE", False)

    assert worker_supervisor.main() == 64


def test_terminal_recycle_exit_stops_supervisor_without_respawn(monkeypatch):
    class ExitedChild:
        pid = 12345

        @staticmethod
        def poll():
            return worker_supervisor.WORKER_RECYCLE_EXIT_CODE

    spawn_calls = 0
    generation_waits = []

    def spawn_child():
        nonlocal spawn_calls
        spawn_calls += 1
        return ExitedChild()

    def wait_for_minimum_generation(generation_started_at, stopping):
        generation_waits.append((generation_started_at, stopping))
        return True

    monkeypatch.setattr(worker_supervisor, "WORKER_TOKEN", "test-token")
    monkeypatch.setattr(worker_supervisor, "PUBLIC_WORKER_MODE", True)
    monkeypatch.setattr(worker_supervisor, "_supervisor_is_root", lambda: True)
    monkeypatch.setattr(worker_supervisor, "_spawn_child", spawn_child)
    monkeypatch.setattr(
        worker_supervisor,
        "_wait_for_minimum_generation_lifetime",
        wait_for_minimum_generation,
    )
    monkeypatch.setattr(worker_supervisor.signal, "signal", lambda *_args: None)
    monkeypatch.setattr(
        worker_supervisor, "_signal_process_group", lambda _process, _signal: None
    )

    assert worker_supervisor.main() == worker_supervisor.WORKER_RECYCLE_EXIT_CODE
    assert spawn_calls == 1
    assert len(generation_waits) == 1


def test_unexpected_worker_exit_fails_container_without_respawn(monkeypatch):
    child = SimpleNamespace(pid=12345, poll=lambda: 1)
    spawn_calls = 0

    def spawn_child():
        nonlocal spawn_calls
        spawn_calls += 1
        return child

    def unexpected_generation_wait(*_args):
        raise AssertionError("exit 70 must retain Docker restart backoff")

    monkeypatch.setattr(worker_supervisor, "WORKER_TOKEN", "test-token")
    monkeypatch.setattr(worker_supervisor, "PUBLIC_WORKER_MODE", True)
    monkeypatch.setattr(worker_supervisor, "_supervisor_is_root", lambda: True)
    monkeypatch.setattr(worker_supervisor, "_spawn_child", spawn_child)
    monkeypatch.setattr(
        worker_supervisor,
        "_wait_for_minimum_generation_lifetime",
        unexpected_generation_wait,
    )
    monkeypatch.setattr(worker_supervisor.signal, "signal", lambda *_args: None)
    monkeypatch.setattr(
        worker_supervisor, "_signal_process_group", lambda _process, _signal: None
    )

    assert worker_supervisor.main() == 70
    assert spawn_calls == 1


def test_terminal_recycle_waits_only_for_remaining_generation_lifetime(monkeypatch):
    waits = []

    class NotStopping:
        def wait(self, timeout):
            waits.append(timeout)
            return False

    monkeypatch.setattr(worker_supervisor, "MIN_GENERATION_SECONDS", 11.0)
    monkeypatch.setattr(worker_supervisor.time, "monotonic", lambda: 103.25)

    assert (
        worker_supervisor._wait_for_minimum_generation_lifetime(
            100.0,
            NotStopping(),
        )
        is True
    )
    assert waits == [7.75]


def test_spawned_child_receives_supervisor_generated_boot_id(monkeypatch):
    observed = {}
    worker_secret = os.urandom(16).hex()

    def fake_popen(command, **kwargs):
        observed["command"] = command
        observed.update(kwargs)
        observed["token"] = os.read(kwargs["pass_fds"][0], 4097).decode()
        return SimpleNamespace(pid=12345)

    monkeypatch.setattr(worker_supervisor, "WORKER_TOKEN", worker_secret)
    monkeypatch.setattr(worker_supervisor.subprocess, "Popen", fake_popen)

    worker_supervisor._spawn_child()

    assert observed["env"]["PDF2ZH_CONTAINER_BOOT_ID"] == (
        worker_supervisor.CONTAINER_BOOT_ID
    )
    assert "PDF2ZH_WORKER_TOKEN" not in observed["env"]
    assert observed["env"]["PDF2ZH_WORKER_TOKEN_FD"] == str(observed["pass_fds"][0])
    assert observed["token"] == worker_secret
    assert observed["user"] == worker_supervisor.APP_UID
    assert observed["group"] == worker_supervisor.APP_GID
    assert observed["extra_groups"] == ()
    assert observed["umask"] == 0o077
    assert observed["start_new_session"] is True
    assert observed["command"][-1] == "--no-access-log"


def test_supervisor_verifies_child_uid_gid_and_empty_capability_sets(monkeypatch):
    status = """\
Uid:\t10001\t10001\t10001\t10001
Gid:\t10001\t10001\t10001\t10001
CapInh:\t0000000000000000
CapPrm:\t0000000000000000
CapEff:\t0000000000000000
CapAmb:\t0000000000000000
NoNewPrivs:\t1
"""

    class StatusPath:
        def read_text(self):
            return status

    monkeypatch.setattr(worker_supervisor, "Path", lambda _path: StatusPath())
    process = SimpleNamespace(pid=12345)

    assert worker_supervisor._child_has_unprivileged_identity(process) is True

    status = status.replace("CapEff:\t0000000000000000", "CapEff:\t0000000000000020")
    assert worker_supervisor._child_has_unprivileged_identity(process) is False


def test_supervisor_health_binds_epoch_to_its_own_boot_id(monkeypatch):
    class Response(io.BytesIO):
        def __enter__(self):
            return self

        def __exit__(self, *_args):
            self.close()

    def response_for(boot_id):
        payload = {
            "status": "ok",
            "provider": "google",
            "max_active_jobs": 1,
            "container_boot_id": boot_id,
            "epoch": "worker-epoch",
        }
        return Response(json.dumps(payload).encode())

    monkeypatch.setattr(
        worker_supervisor.urllib.request,
        "urlopen",
        lambda *_args, **_kwargs: response_for(worker_supervisor.CONTAINER_BOOT_ID),
    )
    assert worker_supervisor._healthy() is True

    monkeypatch.setattr(
        worker_supervisor.urllib.request,
        "urlopen",
        lambda *_args, **_kwargs: response_for("stale-container"),
    )
    assert worker_supervisor._healthy() is False
