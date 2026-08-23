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
        "results": {},
        "file_order": [],
        "display_map": {},
        "parent_map": {},
        "uploaded_files": [],
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
        gui.logo_path.resolve(),
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
