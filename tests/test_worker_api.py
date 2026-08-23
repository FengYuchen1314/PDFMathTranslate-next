import asyncio
from pathlib import Path
from types import SimpleNamespace

import pytest
from pdf2zh_next import worker_api
from pdf2zh_next.config.translate_engine_model import GoogleSettings
from pdf2zh_next.high_level import UnrecoverableCleanupError


def make_request(input_path: Path, output_dir: Path, **overrides):
    values = {
        "input_path": str(input_path),
        "output_dir": str(output_dir),
        "source_lang": "auto",
        "target_lang": "zh",
        "pages": None,
        "output_mono": True,
        "output_dual": True,
    }
    values.update(overrides)
    return worker_api.TranslationRequest(**values)


def test_worker_settings_are_google_only(tmp_path):
    request = make_request(tmp_path / "input.pdf", tmp_path / "output")
    settings = worker_api._build_settings(request, tmp_path / "output")

    assert isinstance(settings.translate_engine_settings, GoogleSettings)
    assert settings.term_extraction_engine_settings is None
    assert settings.translation.no_auto_extract_glossary is True
    assert settings.pdf.watermark_output_mode == "no_watermark"


def test_worker_rejects_auto_target_language(tmp_path):
    with pytest.raises(ValueError):
        make_request(tmp_path / "input.pdf", tmp_path / "output", target_lang="auto")


def test_worker_paths_are_locked_to_the_job_directory(tmp_path, monkeypatch):
    job_id = "f6a62ca0-39a7-44e4-9bf6-413758c16931"
    job_root = tmp_path / job_id
    job_root.mkdir()
    input_path = job_root / "input.pdf"
    input_path.write_bytes(b"%PDF-1.7\n")
    output_dir = job_root / "output"
    monkeypatch.setattr(worker_api, "DATA_ROOT", tmp_path.resolve())

    request = make_request(input_path, output_dir)
    assert worker_api._validated_paths(job_id, request) == (
        input_path.resolve(),
        output_dir.resolve(),
    )

    escaped = make_request(input_path, tmp_path / "elsewhere")
    with pytest.raises(worker_api.HTTPException):
        worker_api._validated_paths(job_id, escaped)


def test_cancellation_event_is_emitted_after_stream_cleanup(tmp_path, monkeypatch):
    cleanup_done = asyncio.Event()
    hold = asyncio.Event()

    async def fake_translate(_settings, _input_path):
        try:
            yield {
                "type": "progress_update",
                "overall_progress": 20,
                "stage": "Translating",
            }
            await hold.wait()
        finally:
            await asyncio.sleep(0)
            cleanup_done.set()

    monkeypatch.setattr(worker_api, "do_translate_async_stream", fake_translate)

    async def run():
        input_path = tmp_path / "input.pdf"
        input_path.write_bytes(b"%PDF-1.7\n")
        output_dir = tmp_path / "output"
        request = make_request(input_path, output_dir)
        job = worker_api.WorkerJob(job_id="job", request=request)
        task = asyncio.create_task(
            worker_api._run_translation(job, input_path, output_dir)
        )
        while len(job.events) < 2:
            await asyncio.sleep(0)
        task.cancel()
        await task
        assert cleanup_done.is_set()
        assert job.events[-1]["type"] == "cancelled"
        assert job.terminal is True

    asyncio.run(run())


def test_finished_results_are_materialized_with_stable_names(tmp_path, monkeypatch):
    cleanup_done = asyncio.Event()

    async def fake_translate(_settings, _input_path):
        try:
            generated = tmp_path / "generated.pdf"
            generated.write_bytes(b"%PDF-1.7\ntranslated")
            yield {
                "type": "finish",
                "translate_result": SimpleNamespace(
                    no_watermark_mono_pdf_path=generated,
                    no_watermark_dual_pdf_path=None,
                    mono_pdf_path=None,
                    dual_pdf_path=None,
                    auto_extracted_glossary_path=None,
                ),
            }
        finally:
            await asyncio.sleep(0)
            cleanup_done.set()

    monkeypatch.setattr(worker_api, "do_translate_async_stream", fake_translate)

    async def run():
        input_path = tmp_path / "input.pdf"
        input_path.write_bytes(b"%PDF-1.7\n")
        output_dir = tmp_path / "output"
        request = make_request(
            input_path, output_dir, output_mono=True, output_dual=False
        )
        job = worker_api.WorkerJob(job_id="job", request=request)
        original_emit = job.emit

        async def assert_cleanup_before_terminal(event):
            if event.get("type") in worker_api.TERMINAL_EVENTS:
                assert cleanup_done.is_set()
            await original_emit(event)

        job.emit = assert_cleanup_before_terminal
        await worker_api._run_translation(job, input_path, output_dir)

        assert job.events[-1]["type"] == "finished"
        assert job.events[-1]["files"] == {
            "mono": True,
            "dual": False,
            "glossary": False,
        }
        assert (output_dir / "mono.pdf").read_bytes().startswith(b"%PDF-")

    asyncio.run(run())


def test_unproven_subprocess_cleanup_uses_fail_stop_without_terminal(
    tmp_path, monkeypatch
):
    class FailStopObservedError(RuntimeError):
        pass

    async def fake_translate(_settings, _input_path):
        raise UnrecoverableCleanupError("process group remains alive")
        yield  # pragma: no cover - keeps this function an async generator

    def fake_fail_stop(_job_id, _error):
        raise FailStopObservedError

    monkeypatch.setattr(worker_api, "do_translate_async_stream", fake_translate)
    monkeypatch.setattr(worker_api, "_fail_stop_worker", fake_fail_stop)

    async def run():
        input_path = tmp_path / "input.pdf"
        input_path.write_bytes(b"%PDF-1.7\n")
        output_dir = tmp_path / "output"
        request = make_request(input_path, output_dir)
        job = worker_api.WorkerJob(job_id="job", request=request)

        with pytest.raises(FailStopObservedError):
            await worker_api._run_translation(job, input_path, output_dir)
        assert job.terminal is False
        assert job.cleanup_complete.is_set() is False

    asyncio.run(run())


def test_first_terminal_worker_event_wins(tmp_path):
    async def run():
        request = make_request(tmp_path / "input.pdf", tmp_path / "output")
        job = worker_api.WorkerJob(job_id="job", request=request)
        await job.emit({"type": "finished", "files": {"mono": True}})
        await job.emit({"type": "cancelled"})

        assert [event["type"] for event in job.events] == ["finished"]
        assert job.status == "finished"

    asyncio.run(run())


def test_terminal_journal_survives_worker_registry_loss(tmp_path, monkeypatch):
    job_id = "6455ee52-7ce4-4424-901f-3e197e4ad458"
    job_root = tmp_path / job_id
    output_dir = job_root / "output"
    monkeypatch.setattr(worker_api, "DATA_ROOT", tmp_path.resolve())

    async def run():
        worker_api._jobs.clear()
        request = make_request(job_root / "input.pdf", output_dir)
        job = worker_api.WorkerJob(
            job_id=job_id,
            request=request,
            terminal_journal_path=(output_dir / worker_api.TERMINAL_JOURNAL_FILENAME),
        )
        worker_api._jobs[job_id] = job
        await job.emit({"type": "progress", "progress": 50})

        with pytest.raises(worker_api.HTTPException) as exc_info:
            await worker_api.job_terminal(job_id, None)
        assert exc_info.value.status_code == 409

        terminal = {
            "type": "finished",
            "progress": 100,
            "stage": "翻译完成",
            "message": "文件已整理，可以下载",
            "files": {"mono": True, "dual": False, "glossary": False},
        }
        await job.emit(terminal)
        response = await worker_api.job_terminal(job_id, None)

        assert response == {"job_id": job_id, **terminal}
        worker_api._jobs.clear()
        recovered = await worker_api.job_terminal(job_id, None)
        assert recovered == response

        retry = await worker_api.start_job(
            job_id,
            make_request(tmp_path / "missing.pdf", tmp_path / "missing-output"),
            None,
        )
        assert retry.status == "finished"
        assert job_id not in worker_api._jobs

    asyncio.run(run())


def test_terminal_journal_write_failure_uses_fail_stop(tmp_path, monkeypatch):
    class FailStopObservedError(RuntimeError):
        pass

    def fail_persist(_path, _event):
        raise OSError("disk unavailable")

    def fake_fail_stop(_job_id, _error):
        raise FailStopObservedError

    monkeypatch.setattr(worker_api, "_persist_terminal_event", fail_persist)
    monkeypatch.setattr(worker_api, "_fail_stop_worker", fake_fail_stop)

    async def run():
        request = make_request(tmp_path / "input.pdf", tmp_path / "output")
        job = worker_api.WorkerJob(
            job_id="job",
            request=request,
            terminal_journal_path=tmp_path / "output" / "terminal.json",
        )

        with pytest.raises(FailStopObservedError):
            await job.emit({"type": "finished"})
        assert job.terminal is False
        assert job.cleanup_complete.is_set() is False

    asyncio.run(run())


def test_corrupt_terminal_journal_converges_after_registry_loss(tmp_path, monkeypatch):
    job_id = "d449ae5f-98cb-48a8-82f9-e80f684cfb2e"
    job_root = tmp_path / job_id
    output_dir = job_root / "output"
    journal = output_dir / worker_api.TERMINAL_JOURNAL_FILENAME
    output_dir.mkdir(parents=True)
    journal.write_bytes(b"{not-json")
    monkeypatch.setattr(worker_api, "DATA_ROOT", tmp_path.resolve())

    async def run():
        worker_api._jobs.clear()
        request = make_request(job_root / "input.pdf", output_dir)
        active = worker_api.WorkerJob(job_id=job_id, request=request)
        worker_api._jobs[job_id] = active

        # An active registry entry owns the ID. Its on-disk journal must not
        # be quarantined by a concurrent status request.
        with pytest.raises(worker_api.HTTPException) as exc_info:
            await worker_api.job_terminal(job_id, None)
        assert exc_info.value.status_code == 409
        assert journal.read_bytes() == b"{not-json"

        worker_api._jobs.clear()
        repaired = await worker_api.job_terminal(job_id, None)
        assert repaired["type"] == "failed"
        assert repaired["job_id"] == job_id
        assert len(list(output_dir.glob(".worker-terminal.corrupt.*"))) == 1

        # A later epoch reads the replacement tombstone and converges on the
        # same outcome instead of returning another 500 or starting work.
        assert await worker_api.job_terminal(job_id, None) == repaired
        recovery = output_dir / worker_api.TERMINAL_RECOVERY_FILENAME
        journal.replace(recovery)
        assert await worker_api.job_terminal(job_id, None) == repaired
        assert journal.is_file()
        assert not recovery.exists()
        retry = await worker_api.start_job(
            job_id,
            make_request(tmp_path / "missing.pdf", tmp_path / "missing-output"),
            None,
        )
        assert retry.status == "failed"

    asyncio.run(run())


def test_cancel_tombstone_prevents_late_ambiguous_start(tmp_path, monkeypatch):
    job_id = "117586fd-c362-4d85-aeb9-15e30aa4dc0f"
    job_root = tmp_path / job_id
    job_root.mkdir()
    input_path = job_root / "input.pdf"
    input_path.write_bytes(b"%PDF-1.7\n")
    output_dir = job_root / "output"
    request = make_request(input_path, output_dir)
    monkeypatch.setattr(worker_api, "DATA_ROOT", tmp_path.resolve())

    async def run():
        worker_api._jobs.clear()
        worker_api._cancelled_before_start.clear()
        worker_api._accepting_jobs = True
        cancelled = await worker_api.cancel_job(job_id, None)
        # Simulate an epoch restart losing the in-memory tombstone.
        worker_api._cancelled_before_start.clear()
        late_start = await worker_api.start_job(job_id, request, None)

        assert cancelled.status == "cancelled"
        assert late_start.status == "cancelled"
        assert job_id not in worker_api._jobs

    asyncio.run(run())


def test_worker_allows_only_one_active_job_and_same_id_is_idempotent(
    tmp_path, monkeypatch
):
    first_id = "022bf41a-77e4-4432-b4c4-2282681e366d"
    second_id = "8e8b0cf5-4ce3-453f-a077-0b56d73088b4"

    def prepare(job_id):
        job_root = tmp_path / job_id
        job_root.mkdir()
        input_path = job_root / "input.pdf"
        input_path.write_bytes(b"%PDF-1.7\n")
        return make_request(input_path, job_root / "output")

    first_request = prepare(first_id)
    second_request = prepare(second_id)
    monkeypatch.setattr(worker_api, "DATA_ROOT", tmp_path.resolve())

    async def fake_run(_job, _input_path, _output_dir):
        await asyncio.Event().wait()

    monkeypatch.setattr(worker_api, "_run_translation", fake_run)

    async def run():
        worker_api._jobs.clear()
        worker_api._cancelled_before_start.clear()
        worker_api._accepting_jobs = True
        started = await worker_api.start_job(first_id, first_request, None)

        # Existing IDs win over drain state and path revalidation. This is the
        # retry path used when the first 202 response was lost in transit.
        worker_api._accepting_jobs = False
        missing_request = make_request(
            tmp_path / "missing.pdf", tmp_path / "missing-output"
        )
        retried = await worker_api.start_job(first_id, missing_request, None)
        assert started.status == "running"
        assert retried.status == "running"

        worker_api._accepting_jobs = True
        with pytest.raises(worker_api.HTTPException) as exc_info:
            await worker_api.start_job(second_id, second_request, None)
        assert exc_info.value.status_code == 409
        assert second_id not in worker_api._jobs

        cancelled = await worker_api.cancel_job(first_id, None)
        assert cancelled.status == "cancelled"
        worker_api._jobs.clear()
        worker_api._cancelled_before_start.clear()

    asyncio.run(run())


def test_worker_event_journal_is_bounded(tmp_path, monkeypatch):
    monkeypatch.setattr(worker_api, "MAX_JOURNAL_EVENTS", 3)

    async def run():
        request = make_request(tmp_path / "input.pdf", tmp_path / "output")
        job = worker_api.WorkerJob(job_id="job", request=request)
        for index in range(5):
            await job.emit({"type": "progress", "progress": index})

        assert job.event_offset == 2
        assert [event["progress"] for event in job.events] == [2, 3, 4]

    asyncio.run(run())


def test_repeated_cancel_requests_inject_one_task_cancellation(tmp_path):
    job_id = "42bf0e82-c926-449b-a150-be9ce663bb12"

    class CountingTask(asyncio.Task):
        cancel_calls = 0

        def cancel(self, *args, **kwargs):
            self.cancel_calls += 1
            return super().cancel(*args, **kwargs)

    async def run():
        worker_api._jobs.clear()
        request = make_request(tmp_path / "input.pdf", tmp_path / "output")
        job = worker_api.WorkerJob(job_id=job_id, request=request)

        async def translation_task():
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                await asyncio.sleep(0)
                await job.emit({"type": "cancelled"})

        task = CountingTask(translation_task())
        job.task = task
        worker_api._jobs[job_id] = job

        first, second = await asyncio.gather(
            worker_api.cancel_job(job_id, None),
            worker_api.cancel_job(job_id, None),
        )

        assert first.status == "cancelled"
        assert second.status == "cancelled"
        assert task.cancel_calls == 1
        assert job.cleanup_complete.is_set()
        worker_api._jobs.clear()

    asyncio.run(run())


def test_startup_drain_cancels_unknown_jobs_before_resume(tmp_path):
    job_id = "87f97861-dbd7-44fb-b232-0982134fa7de"

    async def run():
        worker_api._jobs.clear()
        worker_api._accepting_jobs = True
        request = make_request(tmp_path / "input.pdf", tmp_path / "output")
        job = worker_api.WorkerJob(job_id=job_id, request=request)

        async def translation_task():
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                await job.emit({"type": "cancelled"})

        job.task = asyncio.create_task(translation_task())
        worker_api._jobs[job_id] = job

        drained = await worker_api.drain_worker(None)
        assert drained.status == "idle"
        assert drained.active_jobs == 0
        assert job.status == "cancelled"
        assert worker_api._accepting_jobs is False

        resumed = await worker_api.resume_worker(None)
        assert resumed.status == "ready"
        assert worker_api._accepting_jobs is True
        worker_api._jobs.clear()

    asyncio.run(run())
