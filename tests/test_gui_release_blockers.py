import sys
import tempfile
from pathlib import Path


def _gui(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["pytest"])
    import pdf2zh_next.gui as gui

    return gui


def _empty_state():
    return {
        "session_id": None,
        "current_task": None,
        "worker_task": None,
        "cancel_requested": False,
        "results": {},
        "file_order": [],
        "display_map": {},
        "parent_map": {},
        "uploaded_files": [],
        "run_dirs": [],
    }


def test_pdf_preview_allowed_paths_exclude_broad_filesystem_paths(monkeypatch):
    gui = _gui(monkeypatch)

    allowed_paths = [Path(path).resolve() for path in gui.pdf_preview_allowed_paths]
    root_paths = {Path(path.anchor).resolve() for path in allowed_paths if path.anchor}

    assert Path.cwd().resolve() not in allowed_paths
    assert Path.home().resolve() not in allowed_paths
    assert not root_paths.intersection(allowed_paths)


def test_pdf_preview_allowed_paths_keep_accepted_preview_locations(monkeypatch):
    gui = _gui(monkeypatch)

    allowed_paths = [Path(path).resolve() for path in gui.pdf_preview_allowed_paths]
    expected_paths = {
        Path("pdf2zh_files").resolve(),
        Path(tempfile.gettempdir()).resolve(),
    }

    assert set(allowed_paths) == expected_paths


def test_on_file_upload_empty_list_returns_all_declared_outputs(monkeypatch):
    gui = _gui(monkeypatch)
    state = _empty_state()

    result = gui.on_file_upload([], state)

    assert len(result) == 3
    assert result[0]["choices"] == []
    assert result[0]["value"] is None
    assert result[0]["visible"] is False
    assert result[1] is state
    assert result[2]["value"] == ""
    assert result[2]["visible"] is False


def test_on_file_upload_none_returns_all_declared_outputs(monkeypatch):
    gui = _gui(monkeypatch)
    state = _empty_state()

    result = gui.on_file_upload(None, state)

    assert len(result) == 3
    assert result[0]["choices"] == []
    assert result[0]["value"] is None
    assert result[0]["visible"] is False
    assert result[1] is state
    assert result[2]["value"] == ""
    assert result[2]["visible"] is False


def test_google_is_the_only_available_translation_service(monkeypatch):
    gui = _gui(monkeypatch)

    assert gui.available_services == ["Google"]
    assert gui.term_service.choices == [
        ("Follow main translation engine", "Follow main translation engine")
    ]
    assert "SiliconFlow" not in gui.tech_details_string


def test_browser_heartbeat_has_an_unload_cancellation_handler(monkeypatch):
    gui = _gui(monkeypatch)

    unload_dependencies = [
        dependency
        for dependency in gui.demo.config["dependencies"]
        if any(target[1] == "unload" for target in dependency.get("targets", []))
    ]

    assert gui.demo.config["connect_heartbeat"] is True
    assert len(unload_dependencies) == 1
    assert unload_dependencies[0]["api_name"] == "stop_translation_on_unload"


def test_stop_translation_schedules_cancellation_on_the_task_loop(monkeypatch):
    gui = _gui(monkeypatch)

    class FakeLoop:
        def __init__(self):
            self.callback = None

        def call_soon_threadsafe(self, callback):
            self.callback = callback

    class FakeTask:
        def __init__(self, loop):
            self.loop = loop
            self.cancelled = False

        def get_loop(self):
            return self.loop

        def cancel(self):
            self.cancelled = True

    loop = FakeLoop()
    task = FakeTask(loop)
    state = _empty_state()
    state["current_task"] = task

    assert gui.stop_translate_file(state) is state
    assert state["current_task"] is task
    assert state["cancel_requested"] is True
    loop.callback()
    assert task.cancelled is True


def test_session_registry_allows_unload_to_cancel_without_gradio_state(monkeypatch):
    gui = _gui(monkeypatch)

    class FakeLoop:
        def call_soon_threadsafe(self, callback):
            callback()

    class FakeTask:
        def __init__(self):
            self.cancelled = False
            self.done_callbacks = []

        def get_loop(self):
            return FakeLoop()

        def cancel(self):
            self.cancelled = True

        def add_done_callback(self, callback):
            self.done_callbacks.append(callback)

    class FakeRequest:
        session_hash = "browser-session"

    task = FakeTask()
    state = _empty_state()
    gui._register_translation_task(FakeRequest(), state, task)

    gui.stop_translation_on_unload(FakeRequest())

    assert task.cancelled is True
    assert gui._active_translation_tasks["browser-session"] is task

    for callback in task.done_callbacks:
        callback(task)
    assert "browser-session" not in gui._active_translation_tasks


def test_unload_reclaims_successful_session_outputs_without_gradio_state(
    monkeypatch, tmp_path
):
    gui = _gui(monkeypatch)
    output_root = tmp_path / "outputs"
    run_dir = output_root / "completed-run"
    run_dir.mkdir(parents=True)
    (run_dir / "translated.pdf").write_bytes(b"pdf")
    monkeypatch.setattr(gui, "_OUTPUT_ROOT", output_root)

    class FakeRequest:
        session_hash = "completed-browser-session"

    state = _empty_state()
    gui._track_session_run_directory(FakeRequest(), state, run_dir)

    gui.stop_translation_on_unload(FakeRequest())

    assert not run_dir.exists()
    assert "completed-browser-session" not in gui._session_run_dirs


def test_remove_run_directory_is_scoped_to_output_root(monkeypatch, tmp_path):
    gui = _gui(monkeypatch)
    output_root = tmp_path / "outputs"
    run_dir = output_root / "run-id"
    outside = tmp_path / "outside"
    run_dir.mkdir(parents=True)
    outside.mkdir()
    (run_dir / "partial.pdf").write_bytes(b"partial")
    (outside / "keep.txt").write_text("keep", encoding="utf-8")
    monkeypatch.setattr(gui, "_OUTPUT_ROOT", output_root)

    gui._remove_run_directory(run_dir)
    gui._remove_run_directory(outside)

    assert not run_dir.exists()
    assert outside.exists()
