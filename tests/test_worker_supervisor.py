from pdf2zh_next import worker_supervisor


def test_supervisor_requires_private_worker_token(monkeypatch):
    monkeypatch.setattr(worker_supervisor, "WORKER_TOKEN", "")

    assert worker_supervisor.main() == 64


def test_unexpected_worker_exit_fails_container_without_respawn(monkeypatch):
    class ExitedChild:
        pid = 12345

        @staticmethod
        def poll():
            return 70

    spawn_calls = 0

    def spawn_child():
        nonlocal spawn_calls
        spawn_calls += 1
        return ExitedChild()

    monkeypatch.setattr(worker_supervisor, "WORKER_TOKEN", "test-token")
    monkeypatch.setattr(worker_supervisor, "_spawn_child", spawn_child)
    monkeypatch.setattr(worker_supervisor.signal, "signal", lambda *_args: None)
    monkeypatch.setattr(
        worker_supervisor, "_signal_process_group", lambda _process, _signal: None
    )

    assert worker_supervisor.main() == 70
    assert spawn_calls == 1
