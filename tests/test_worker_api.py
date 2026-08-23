import asyncio
import logging
import os
import threading
from pathlib import Path
from types import SimpleNamespace

import pytest
from pdf2zh_next import worker_api
from pdf2zh_next.config.translate_engine_model import GoogleSettings
from pdf2zh_next.config.translate_engine_model import OpenAISettings
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


def test_worker_defaults_to_google_settings(tmp_path):
    request = make_request(tmp_path / "input.pdf", tmp_path / "output")
    settings = worker_api._build_settings(request, tmp_path / "output")

    assert request.provider == "google"
    assert isinstance(settings.translate_engine_settings, GoogleSettings)
    assert settings.term_extraction_engine_settings is None
    assert settings.translation.no_auto_extract_glossary is True
    assert settings.translation.ignore_cache is True
    assert settings.pdf.watermark_output_mode == "no_watermark"


@pytest.mark.parametrize(
    ("model", "thinking_mode", "reasoning_effort", "sent_reasoning_effort"),
    [
        (None, None, None, None),
        ("deepseek-v4-pro", None, None, None),
        ("deepseek-v4-flash", "disabled", None, None),
        ("deepseek-v4-pro", "disabled", None, None),
        ("deepseek-v4-flash", "enabled", None, None),
        ("deepseek-v4-flash", "enabled", "low", "high"),
        ("deepseek-v4-flash", "enabled", "high", "high"),
        ("deepseek-v4-pro", "enabled", "max", "max"),
    ],
)
def test_worker_builds_supported_deepseek_settings(
    tmp_path,
    model,
    thinking_mode,
    reasoning_effort,
    sent_reasoning_effort,
):
    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="deepseek",
        model=model,
        thinking_mode=thinking_mode,
        reasoning_effort=reasoning_effort,
        openai_api_key="  server-secret  ",
    )

    settings = worker_api._build_settings(request, tmp_path / "output")
    engine = settings.translate_engine_settings

    assert isinstance(engine, OpenAISettings)
    assert engine.openai_model == (model or "deepseek-v4-flash")
    assert engine.openai_api_key == "server-secret"
    assert engine.openai_base_url == "https://api.deepseek.com"
    assert "server-secret" not in repr(engine)
    assert "server-secret" not in repr(settings)
    if thinking_mode is None:
        assert engine._openai_extra_body is None
        assert engine.openai_send_reasoning_effort is None
    else:
        assert engine._openai_extra_body == {
            "thinking": {"type": thinking_mode},
        }
        if thinking_mode == "enabled":
            if sent_reasoning_effort is None:
                assert engine.openai_send_reasoning_effort is None
            else:
                assert engine.openai_send_reasoning_effort is True
            assert engine.openai_reasoning_effort == sent_reasoning_effort
        else:
            assert engine.openai_send_reasoning_effort is None
            assert engine.openai_reasoning_effort is None


def test_worker_does_not_fall_back_to_deepseek_environment_key(tmp_path, monkeypatch):
    monkeypatch.setenv("PDF2ZH_DEEPSEEK_API_KEY", "legacy-environment-secret")

    with pytest.raises(ValueError, match="require an API key"):
        make_request(
            tmp_path / "input.pdf",
            tmp_path / "output",
            provider="deepseek",
        )


def test_worker_builds_openai_compatible_settings(tmp_path, monkeypatch):
    monkeypatch.setattr(
        worker_api.socket,
        "getaddrinfo",
        lambda *_args, **_kwargs: [
            (
                worker_api.socket.AF_INET,
                worker_api.socket.SOCK_STREAM,
                6,
                "",
                ("93.184.216.34", 443),
            )
        ],
    )
    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="openai_compatible",
        provider_name="  Example Provider  ",
        model="vendor/model  v1",
        reasoning_effort="low",
        openai_base_url="https://api.example.com/openai/v1/",
        openai_api_key="  generic-secret  ",
    )

    settings = worker_api._build_settings(request, tmp_path / "output")
    engine = settings.translate_engine_settings

    assert isinstance(engine, OpenAISettings)
    assert request.provider_name == "Example Provider"
    assert engine.openai_model == "vendor/model  v1"
    assert engine.openai_base_url == "https://api.example.com/openai/v1"
    assert engine.openai_api_key == "generic-secret"
    assert "generic-secret" not in repr(engine)
    assert "generic-secret" not in repr(settings)
    assert engine.openai_send_reasoning_effort is True
    assert engine.openai_reasoning_effort == "low"


@pytest.mark.parametrize(
    "overrides",
    [
        {"provider": "deepseek"},
        {"provider": "deepseek", "openai_api_key": "   "},
        {"model": "deepseek-v4-flash"},
        {
            "provider": "deepseek",
            "model": "deepseek-chat",
            "openai_api_key": "secret",
        },
        {
            "provider": "deepseek",
            "model": "deepseek-v4-flash",
            "reasoning_effort": "high",
            "openai_api_key": "secret",
        },
        {
            "provider": "deepseek",
            "openai_base_url": "https://api.deepseek.com",
            "openai_api_key": "secret",
        },
        {
            "provider": "google",
            "model": "unexpected",
        },
        {
            "provider": "openai_compatible",
            "model": "gpt-test",
            "openai_api_key": "secret",
        },
        {
            "provider": "openai_compatible",
            "openai_base_url": "https://api.example.com/v1",
            "openai_api_key": "secret",
        },
        {
            "provider": "openai_compatible",
            "model": "gpt-test",
            "openai_base_url": "https://api.example.com/v1",
        },
        {
            "provider": "openai_compatible",
            "model": "gpt-test",
            "thinking_mode": "enabled",
            "openai_base_url": "https://api.example.com/v1",
            "openai_api_key": "secret",
        },
    ],
)
def test_worker_rejects_invalid_provider_option_combinations(
    tmp_path, monkeypatch, overrides
):
    monkeypatch.setattr(
        worker_api.socket,
        "getaddrinfo",
        lambda *_args, **_kwargs: [
            (
                worker_api.socket.AF_INET,
                worker_api.socket.SOCK_STREAM,
                6,
                "",
                ("93.184.216.34", 443),
            )
        ],
    )
    with pytest.raises(ValueError):
        make_request(tmp_path / "input.pdf", tmp_path / "output", **overrides)


def test_worker_request_secret_is_masked_and_422_never_echoes_input(tmp_path):
    schema = worker_api.TranslationRequest.model_json_schema()
    properties = schema["properties"]

    assert "api_key" not in properties
    assert "base_url" not in properties
    request = make_request(tmp_path / "input.pdf", tmp_path / "output")
    assert request.openai_api_key is None
    assert request.openai_base_url is None
    secret = "sk-current-request-secret-12345678"  # noqa: S105
    configured = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="deepseek",
        model="deepseek-v4-flash",
        openai_api_key=secret,
    )
    assert configured.openai_api_key.get_secret_value() == secret
    assert secret not in repr(configured)
    assert "openai_api_key" not in configured.model_dump()
    assert secret not in str(configured.model_dump())
    assert secret not in configured.model_dump_json()
    assert secret not in repr(worker_api.WorkerJob(job_id="job", request=configured))

    with pytest.raises(ValueError) as exc_info:
        make_request(
            tmp_path / "input.pdf",
            tmp_path / "output",
            provider="google",
            openai_api_key=secret,
        )
    assert secret not in str(exc_info.value)
    request_error = worker_api.RequestValidationError(exc_info.value.errors())
    response = asyncio.run(
        worker_api.sanitized_request_validation_error(None, request_error)
    )
    assert secret.encode() not in response.body
    assert b'"input"' not in response.body

    with pytest.raises(ValueError) as extra_info:
        make_request(
            tmp_path / "input.pdf",
            tmp_path / "output",
            deepseek_api_key=secret,
        )
    extra_error = worker_api.RequestValidationError(extra_info.value.errors())
    extra_response = asyncio.run(
        worker_api.sanitized_request_validation_error(None, extra_error)
    )
    assert secret.encode() not in extra_response.body
    assert b'"input"' not in extra_response.body


def test_worker_health_reports_all_internal_engine_capabilities():
    result = asyncio.run(worker_api.health(None))

    assert result["provider"] == "google"
    assert result["providers"] == ["google", "deepseek", "openai_compatible"]
    assert result["max_active_jobs"] == 1
    assert result["epoch"] == worker_api.WORKER_EPOCH
    assert result["container_boot_id"] == worker_api.CONTAINER_BOOT_ID


def test_public_worker_requires_single_job_and_supervisor_boot_id(monkeypatch):
    monkeypatch.setenv("PDF2ZH_MAX_ACTIVE_JOBS", "2")
    with pytest.raises(RuntimeError, match="must be exactly 1"):
        worker_api._validate_runtime_configuration()

    monkeypatch.setenv("PDF2ZH_MAX_ACTIVE_JOBS", "1")
    monkeypatch.setattr(worker_api, "PUBLIC_WORKER_MODE", True)
    monkeypatch.setattr(worker_api, "_worker_process_has_app_identity", lambda: True)
    monkeypatch.setattr(worker_api, "_CONFIGURED_CONTAINER_BOOT_ID", "")
    with pytest.raises(RuntimeError, match="worker supervisor"):
        worker_api._validate_runtime_configuration()

    monkeypatch.setattr(
        worker_api,
        "_CONFIGURED_CONTAINER_BOOT_ID",
        "bb166a87-51a7-4b79-8de4-f717cbfa5bd8",
    )
    worker_api._validate_runtime_configuration()


def test_worker_token_pipe_is_consumed_and_not_left_for_translation_spawn(monkeypatch):
    token_read_fd, token_write_fd = os.pipe()
    try:
        os.write(token_write_fd, b"coordinator-secret")
    finally:
        os.close(token_write_fd)
    monkeypatch.setenv("PDF2ZH_WORKER_TOKEN_FD", str(token_read_fd))
    monkeypatch.setenv("PDF2ZH_WORKER_TOKEN", "legacy-secret")

    assert worker_api._consume_worker_token() == "coordinator-secret"
    assert "PDF2ZH_WORKER_TOKEN_FD" not in os.environ
    assert "PDF2ZH_WORKER_TOKEN" not in os.environ


def test_retiring_public_worker_rejects_new_job_before_route_parsing(monkeypatch):
    request = SimpleNamespace(
        method="POST",
        url=SimpleNamespace(path="/internal/jobs/new-job-id"),
    )
    call_next_called = False

    async def call_next(_request):
        nonlocal call_next_called
        call_next_called = True
        raise AssertionError("retiring worker delegated to request body parsing")

    monkeypatch.setattr(worker_api, "PUBLIC_WORKER_MODE", True)
    monkeypatch.setattr(worker_api, "_retire_requested", True)

    response = asyncio.run(
        worker_api.reject_job_body_after_terminal(request, call_next)
    )

    assert response.status_code == 503
    assert call_next_called is False


def test_public_error_maps_upstream_text_to_fixed_categories(tmp_path):
    configured_key = "sk-configured-secret-12345678"
    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="deepseek",
        openai_api_key=configured_key,
    )

    authentication = worker_api._public_error(
        "request failed "
        f"api_key={configured_key} "
        "Authorization: Bearer sk-header-secret-87654321 "
        "access_token=another-sensitive-token",
        request,
    )

    assert authentication == "翻译渠道认证失败，请联系管理员检查密钥"
    arbitrary = worker_api._public_error(
        "upstream echoed a confidential paragraph from the uploaded paper",
        request,
    )
    assert arbitrary == "翻译服务暂时无法完成任务，请稍后重试"
    assert configured_key not in authentication
    assert "confidential paragraph" not in arbitrary


@pytest.mark.parametrize(
    "base_url",
    [
        "http://api.example.com/v1",
        "https://user:password@api.example.com/v1",
        "https://api.example.com/v1?target=http://127.0.0.1",
        "https://api.example.com/v1#fragment",
        "https://localhost/v1",
        "https://service.internal/v1",
        "https://127%2e0%2e0%2e1/v1",
        "https://127.0.0.1/v1",
        "https://10.0.0.8/v1",
        "https://169.254.169.254/latest/meta-data",
        "https://[::1]/v1",
        "https://0.0.0.0/v1",
        "https://224.0.0.1/v1",
    ],
)
def test_openai_compatible_rejects_unsafe_base_urls(tmp_path, base_url):
    with pytest.raises(ValueError):
        make_request(
            tmp_path / "input.pdf",
            tmp_path / "output",
            provider="openai_compatible",
            model="gpt-test",
            openai_base_url=base_url,
            openai_api_key="secret",
        )


def test_openai_compatible_rejects_hostname_resolving_private(tmp_path, monkeypatch):
    monkeypatch.setattr(
        worker_api.socket,
        "getaddrinfo",
        lambda *_args, **_kwargs: [
            (
                worker_api.socket.AF_INET,
                worker_api.socket.SOCK_STREAM,
                6,
                "",
                ("192.168.1.10", 443),
            )
        ],
    )

    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="openai_compatible",
        model="gpt-test",
        openai_base_url="https://api.example.com/v1",
        openai_api_key="secret",
    )
    with pytest.raises(ValueError, match="resolve exclusively to public addresses"):
        asyncio.run(worker_api._validate_openai_endpoint_for_job(request))


def test_openai_compatible_fails_closed_on_dns_failure(tmp_path, monkeypatch):
    def unavailable_dns(*_args, **_kwargs):
        raise worker_api.socket.gaierror("temporary resolver failure")

    monkeypatch.setattr(worker_api.socket, "getaddrinfo", unavailable_dns)

    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="openai_compatible",
        model="gpt-test",
        openai_base_url="https://api.example.com/v1/",
        openai_api_key="secret",
    )
    with pytest.raises(ValueError, match="resolve exclusively to public addresses"):
        asyncio.run(worker_api._validate_openai_endpoint_for_job(request))


def test_openai_compatible_fails_closed_on_empty_dns_answer(tmp_path, monkeypatch):
    monkeypatch.setattr(
        worker_api.socket,
        "getaddrinfo",
        lambda *_args, **_kwargs: [],
    )

    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="openai_compatible",
        model="gpt-test",
        openai_base_url="https://api.example.com/v1",
        openai_api_key="secret",
    )
    with pytest.raises(ValueError, match="resolve exclusively to public addresses"):
        asyncio.run(worker_api._validate_openai_endpoint_for_job(request))


def test_openai_compatible_rechecks_dns_when_job_starts(tmp_path, monkeypatch):
    answers = iter(["192.168.1.10"])

    def changing_dns(*_args, **_kwargs):
        address = next(answers)
        return [
            (
                worker_api.socket.AF_INET,
                worker_api.socket.SOCK_STREAM,
                6,
                "",
                (address, 443),
            )
        ]

    monkeypatch.setattr(worker_api.socket, "getaddrinfo", changing_dns)
    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="openai_compatible",
        model="gpt-test",
        openai_base_url="https://api.example.com/v1",
        openai_api_key="secret",
    )

    # Construction consumes no DNS response and cannot block FastAPI's event
    # loop. The job-start authority performs the first and final app-level
    # lookup immediately before creating the HTTP client.
    with pytest.raises(ValueError, match="resolve exclusively to public addresses"):
        asyncio.run(worker_api._validate_openai_endpoint_for_job(request))


def test_openai_compatible_dns_timeout_requires_worker_recycle(tmp_path, monkeypatch):
    release_resolver = threading.Event()

    def stuck_resolver(*_args, **_kwargs):
        release_resolver.wait(1)
        return True

    monkeypatch.setattr(
        worker_api,
        "_resolved_addresses_are_public",
        stuck_resolver,
    )
    monkeypatch.setattr(worker_api, "DNS_RESOLUTION_TIMEOUT_SECONDS", 0.01)
    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="openai_compatible",
        model="gpt-test",
        openai_base_url="https://api.example.com/v1",
        openai_api_key="secret",
    )

    async def run():
        try:
            with pytest.raises(UnrecoverableCleanupError):
                await worker_api._validate_openai_endpoint_for_job(request)
        finally:
            release_resolver.set()

    asyncio.run(run())


def test_terminal_journal_never_persists_current_request_secret(
    tmp_path,
):
    secret = "sk-terminal-secret-12345678"  # noqa: S105
    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="deepseek",
        openai_api_key=secret,
    )
    terminal_path = tmp_path / "output" / worker_api.TERMINAL_JOURNAL_FILENAME
    job = worker_api.WorkerJob(
        job_id="560c2e2d-f17a-4052-9273-a5a956933a7d",
        request=request,
        terminal_journal_path=terminal_path,
    )

    asyncio.run(
        job.emit(
            {
                "type": "failed",
                "stage": "翻译失败",
                "message": f"upstream included api_key={secret}",
                "debug_detail": secret,
            }
        )
    )

    journal = terminal_path.read_text(encoding="utf-8")
    assert secret not in journal
    assert "翻译渠道认证失败" in journal
    assert "debug_detail" not in journal


@pytest.mark.parametrize(
    "terminal_event",
    [
        {"type": "cancelled"},
        {"type": "failed", "message": "provider failed"},
        {
            "type": "finished",
            "files": {"mono": True, "dual": False, "glossary": False},
        },
    ],
)
def test_public_terminal_commit_closes_admission_then_requests_container_exit(
    tmp_path, monkeypatch, terminal_event
):
    terminal_path = tmp_path / "output" / worker_api.TERMINAL_JOURNAL_FILENAME
    request = make_request(tmp_path / "input.pdf", tmp_path / "output")
    job = worker_api.WorkerJob(
        job_id="560c2e2d-f17a-4052-9273-a5a956933a7d",
        request=request,
        terminal_journal_path=terminal_path,
    )
    timers = []

    class PendingExitTimer:
        daemon = False

        def __init__(self, delay, target, args):
            # Construction happens only after the terminal rename and directory
            # fsync return. The journal must already be recoverable here.
            assert (
                worker_api._read_terminal_event(terminal_path, job.job_id)["type"]
                == terminal_event["type"]
            )
            self.delay = delay
            self.target = target
            self.args = args
            self.started = False
            timers.append(self)

        def start(self):
            self.started = True

    monkeypatch.setattr(worker_api, "PUBLIC_WORKER_MODE", True)
    monkeypatch.setattr(worker_api, "_accepting_jobs", True)
    monkeypatch.setattr(worker_api, "_retire_requested", False)
    monkeypatch.setattr(worker_api, "_recycle_scheduled", threading.Event())
    monkeypatch.setattr(worker_api.threading, "Timer", PendingExitTimer)

    asyncio.run(job.emit(terminal_event))

    assert worker_api._accepting_jobs is False
    assert worker_api._retire_requested is True
    assert job.cleanup_complete.is_set()
    assert len(timers) == 1
    assert timers[0].started is True
    assert timers[0].delay == worker_api.WORKER_RECYCLE_DELAY_SECONDS
    assert timers[0].target is worker_api.os._exit
    assert timers[0].args == (worker_api.WORKER_RECYCLE_EXIT_CODE,)
    assert asyncio.run(worker_api.health(None))["status"] == "draining"

    # A repeated terminal publication cannot arm a second exit timer.
    asyncio.run(job.emit({"type": "failed"}))
    assert len(timers) == 1


def test_fail_stop_log_redacts_current_request_secret(tmp_path, monkeypatch, caplog):
    class ExitObservedError(RuntimeError):
        pass

    secret = "sk-fail-stop-secret-12345678"  # noqa: S105
    request = make_request(
        tmp_path / "input.pdf",
        tmp_path / "output",
        provider="deepseek",
        openai_api_key=secret,
    )

    def observe_exit(_code):
        raise ExitObservedError

    monkeypatch.setattr(worker_api.os, "_exit", observe_exit)
    caplog.set_level("CRITICAL", logger=worker_api.__name__)

    with pytest.raises(ExitObservedError):
        worker_api._fail_stop_worker(
            "job",
            RuntimeError(f"cleanup response contained {secret}"),
            request,
        )

    assert secret not in caplog.text
    assert "RuntimeError" in caplog.text


def test_public_worker_suppresses_private_translation_diagnostics():
    private_logger = logging.getLogger("pdf2zh_next.high_level")

    assert private_logger.level == logging.CRITICAL
    assert private_logger.propagate is False


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


def test_terminal_job_drops_retained_provider_secret(tmp_path, monkeypatch):
    async def fake_translate(_settings, _input_path):
        yield {
            "type": "error",
            "error": "provider unavailable",
        }

    monkeypatch.setattr(worker_api, "do_translate_async_stream", fake_translate)

    async def run():
        input_path = tmp_path / "input.pdf"
        input_path.write_bytes(b"%PDF-1.7\n")
        output_dir = tmp_path / "output"
        request = make_request(
            input_path,
            output_dir,
            provider="deepseek",
            model="deepseek-v4-flash",
            openai_api_key="terminal-secret",
        )
        job = worker_api.WorkerJob(job_id="job", request=request)

        await worker_api._run_translation(job, input_path, output_dir)

        assert job.terminal is True
        assert job.events[-1]["type"] == "failed"
        assert job.request.openai_api_key is None

    asyncio.run(run())


def test_cancel_before_first_task_poll_drops_provider_secret(tmp_path):
    async def run():
        started = False

        async def never_polled():
            nonlocal started
            started = True
            await asyncio.sleep(60)

        request = make_request(
            tmp_path / "input.pdf",
            tmp_path / "output",
            provider="deepseek",
            model="deepseek-v4-flash",
            openai_api_key="queued-secret",
        )
        job = worker_api.WorkerJob(job_id="job", request=request)
        job.task = asyncio.create_task(never_polled())

        state = await worker_api._cancel_existing_job(job)

        assert started is False
        assert state.status == "cancelled"
        assert job.terminal is True
        assert job.request.openai_api_key is None

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

    def fake_fail_stop(_job_id, _error, _request=None):
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

    def fake_fail_stop(_job_id, _error, _request=None):
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
