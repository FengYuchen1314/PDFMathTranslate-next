from __future__ import annotations

import asyncio
import ipaddress
import json
import logging
import os
import secrets
import shutil
import socket
import threading
from contextlib import aclosing
from contextlib import asynccontextmanager
from dataclasses import dataclass
from dataclasses import field
from pathlib import Path
from typing import Annotated
from typing import Literal
from typing import TypeAlias
from urllib.parse import SplitResult
from urllib.parse import urlsplit
from urllib.parse import urlunsplit
from uuid import UUID
from uuid import uuid4

from fastapi import Depends
from fastapi import FastAPI
from fastapi import Header
from fastapi import HTTPException
from fastapi import Request
from fastapi import status
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from fastapi.responses import StreamingResponse
from pydantic import BaseModel
from pydantic import ConfigDict
from pydantic import Field
from pydantic import SecretStr
from pydantic import field_validator
from pydantic import model_validator

from pdf2zh_next.config.model import PDFSettings
from pdf2zh_next.config.model import SettingsModel
from pdf2zh_next.config.model import TranslationSettings
from pdf2zh_next.config.translate_engine_model import DeepSeekSettings
from pdf2zh_next.config.translate_engine_model import GoogleSettings
from pdf2zh_next.config.translate_engine_model import OpenAICompatibleSettings
from pdf2zh_next.high_level import UnrecoverableCleanupError
from pdf2zh_next.high_level import do_translate_async_stream

logger = logging.getLogger(__name__)

# The OpenAI SDK and the translation subprocess can include request options,
# upstream response bodies, document text, or tracebacks in log records. The
# public worker exposes only its own sanitized error categories, so discard
# those private diagnostics before they can reach the container log.
for _private_logger_name in (
    "openai",
    "httpx",
    "httpcore",
    "h11",
    "pdf2zh_next.high_level",
):
    _private_logger = logging.getLogger(_private_logger_name)
    _private_logger.setLevel(logging.CRITICAL)
    _private_logger.propagate = False


def _consume_worker_token() -> str:
    raw_fd = os.environ.pop("PDF2ZH_WORKER_TOKEN_FD", "")
    legacy_token = os.environ.pop("PDF2ZH_WORKER_TOKEN", "")
    if raw_fd:
        try:
            token_fd = int(raw_fd)
        except ValueError as exc:
            raise RuntimeError("PDF2ZH_WORKER_TOKEN_FD is invalid") from exc
        try:
            with os.fdopen(token_fd, "rb", closefd=True) as token_pipe:
                encoded_token = token_pipe.read(4097)
        except (OSError, ValueError) as exc:
            raise RuntimeError("PDF2ZH worker token pipe is unreadable") from exc
        if len(encoded_token) > 4096:
            raise RuntimeError("PDF2ZH worker token is too long")
        try:
            return encoded_token.decode()
        except UnicodeDecodeError as exc:
            raise RuntimeError("PDF2ZH worker token is not UTF-8") from exc
    # Direct development/test launches retain the legacy environment input.
    # Pop it immediately so a later spawn translation process cannot inherit
    # the coordinator credential.
    return legacy_token


DATA_ROOT = Path(os.getenv("PDF2ZH_DATA_ROOT", "/data/jobs")).resolve()
WORKER_TOKEN = _consume_worker_token()
PUBLIC_WORKER_MODE = os.getenv("PDF2ZH_PUBLIC_WORKER_MODE", "") == "1"
_CONFIGURED_CONTAINER_BOOT_ID = os.environ.pop("PDF2ZH_CONTAINER_BOOT_ID", "")
CONTAINER_BOOT_ID = _CONFIGURED_CONTAINER_BOOT_ID or f"unmanaged-{uuid4()}"
JOB_RETENTION_SECONDS = max(
    60, int(os.getenv("PDF2ZH_WORKER_JOB_RETENTION_SECONDS", "21600"))
)
MAX_JOURNAL_EVENTS = max(
    1000, int(os.getenv("PDF2ZH_WORKER_MAX_JOURNAL_EVENTS", "10000"))
)
CANCEL_HARD_TIMEOUT_SECONDS = max(
    5, int(os.getenv("PDF2ZH_CANCEL_HARD_TIMEOUT_SECONDS", "75"))
)
DNS_RESOLUTION_TIMEOUT_SECONDS = max(
    1.0, float(os.getenv("PDF2ZH_DNS_RESOLUTION_TIMEOUT_SECONDS", "5"))
)
SUPPORTED_LANGUAGES = {
    "auto",
    "en",
    "zh",
    "zh-TW",
    "ja",
    "ko",
    "fr",
    "de",
    "es",
    "ru",
    "it",
    "pt",
}
TERMINAL_EVENTS = {"finished", "failed", "cancelled"}
TERMINAL_JOURNAL_FILENAME = ".worker-terminal.json"
TERMINAL_RECOVERY_FILENAME = ".worker-terminal.recovery.json"
MAX_TERMINAL_JOURNAL_BYTES = 64 * 1024
WORKER_EPOCH = str(uuid4())
WORKER_RECYCLE_EXIT_CODE = 75
# Give a waiting HTTP event stream a best-effort opportunity to flush the
# terminal line. Correctness never depends on this delay: the fsync'd journal
# is the cross-epoch source of truth.
WORKER_RECYCLE_DELAY_SECONDS = 0.05
DEEPSEEK_MODELS = {"deepseek-v4-flash", "deepseek-v4-pro"}
LOCAL_HOSTNAME_SUFFIXES = (".internal", ".local", ".localhost")
PUBLIC_FAILURE_MESSAGES = {
    "翻译渠道认证失败，请联系管理员检查密钥",
    "翻译渠道请求过于频繁或额度不足，请稍后重试",
    "翻译渠道响应超时，请稍后重试",
    "无法连接翻译渠道，请稍后重试",
    "翻译渠道拒绝了请求，请联系管理员检查模型配置",
    "翻译服务暂时无法完成任务，请稍后重试",
}


def _address_is_public(value: str) -> bool:
    try:
        address = ipaddress.ip_address(value)
    except ValueError:
        return False
    return address.is_global and not (
        address.is_private
        or address.is_loopback
        or address.is_link_local
        or address.is_multicast
        or address.is_unspecified
        or address.is_reserved
    )


def _resolved_addresses_are_public(hostname: str, port: int) -> bool:
    try:
        answers = socket.getaddrinfo(
            hostname,
            port,
            type=socket.SOCK_STREAM,
        )
    except (OSError, UnicodeError):
        # Provider endpoints are administrator supplied. Resolver failure must
        # fail closed rather than defer an unchecked hostname to HTTPX.
        return False
    addresses = {answer[4][0].split("%", 1)[0] for answer in answers}
    return bool(addresses) and all(_address_is_public(address) for address in addresses)


def _normalized_openai_base_url(value: str) -> str:
    raw = value.strip()
    if not raw or any(ord(character) < 0x20 for character in raw) or "\\" in raw:
        raise ValueError("OpenAI-compatible base URL is invalid")
    try:
        parsed = urlsplit(raw)
        _validated_port = parsed.port  # force malformed/out-of-range validation
    except ValueError as exc:
        raise ValueError("OpenAI-compatible base URL is invalid") from exc
    if parsed.scheme.lower() != "https":
        raise ValueError("OpenAI-compatible base URL must use HTTPS")
    if parsed.username is not None or parsed.password is not None:
        raise ValueError("OpenAI-compatible base URL cannot contain user info")
    if parsed.query or parsed.fragment:
        raise ValueError("OpenAI-compatible base URL cannot contain query or fragment")
    if not parsed.hostname:
        raise ValueError("OpenAI-compatible base URL requires a host")
    if "%" in parsed.hostname:
        raise ValueError("OpenAI-compatible base URL host cannot be percent-encoded")
    try:
        hostname = parsed.hostname.encode("idna").decode("ascii").rstrip(".").lower()
    except UnicodeError as exc:
        raise ValueError("OpenAI-compatible base URL host is invalid") from exc
    if not hostname:
        raise ValueError("OpenAI-compatible base URL requires a host")
    if hostname == "localhost" or hostname.endswith(LOCAL_HOSTNAME_SUFFIXES):
        raise ValueError("OpenAI-compatible base URL cannot target a local host")
    try:
        literal_address = ipaddress.ip_address(hostname.split("%", 1)[0])
    except ValueError:
        literal_address = None
    if literal_address is not None:
        if not _address_is_public(str(literal_address)):
            raise ValueError(
                "OpenAI-compatible base URL cannot target a private address"
            )
    normalized_host = f"[{hostname}]" if ":" in hostname else hostname
    netloc = (
        normalized_host if parsed.port is None else f"{normalized_host}:{parsed.port}"
    )
    path = parsed.path.rstrip("/")
    normalized = SplitResult("https", netloc, path, "", "")
    return urlunsplit(normalized)


async def _validate_openai_endpoint_for_job(request: TranslationRequest) -> None:
    if request.provider != "openai_compatible" or not request.openai_base_url:
        return
    parsed = urlsplit(request.openai_base_url)
    hostname = parsed.hostname or ""
    try:
        ipaddress.ip_address(hostname)
    except ValueError:
        port = parsed.port or 443
        try:
            is_public = await asyncio.wait_for(
                asyncio.to_thread(_resolved_addresses_are_public, hostname, port),
                timeout=DNS_RESOLUTION_TIMEOUT_SECONDS,
            )
        except asyncio.TimeoutError as exc:
            # Cancelling asyncio.to_thread cannot stop a libc resolver call.
            # Treat the worker epoch as contaminated so the supervisor replaces
            # its process/cgroup instead of accumulating stuck resolver threads.
            raise UnrecoverableCleanupError(
                "DNS resolution exceeded its cleanup-safe deadline"
            ) from exc
        if not is_public:
            raise ValueError(
                "OpenAI-compatible base URL must resolve exclusively to public addresses"
            ) from None


class TranslationRequest(BaseModel):
    # This is an authenticated internal endpoint. Unknown fields are rejected,
    # while the exception handler below removes Pydantic's original input from
    # every 422 response so credentials can never be echoed.
    model_config = ConfigDict(extra="forbid", hide_input_in_errors=True)

    input_path: str
    output_dir: str
    source_lang: str = "auto"
    target_lang: str = "zh"
    pages: str | None = Field(default=None, max_length=256)
    output_mono: bool = True
    output_dual: bool = True
    provider: Literal["google", "deepseek", "openai_compatible"] = "google"
    model: str | None = Field(default=None, max_length=256)
    thinking_mode: Literal["enabled", "disabled"] | None = None
    reasoning_effort: Literal["low", "high", "max"] | None = None
    provider_name: str | None = Field(default=None, max_length=120)
    openai_base_url: str | None = Field(default=None, max_length=2048)
    openai_api_key: SecretStr | None = Field(default=None, exclude=True, repr=False)

    @field_validator("source_lang", "target_lang")
    @classmethod
    def validate_language(cls, value: str, info) -> str:
        if value not in SUPPORTED_LANGUAGES:
            raise ValueError("unsupported language")
        if info.field_name == "target_lang" and value == "auto":
            raise ValueError("target language cannot be auto")
        return value

    @field_validator("pages")
    @classmethod
    def normalize_pages(cls, value: str | None) -> str | None:
        if value is None or not value.strip():
            return None
        value = value.replace(" ", "")
        for part in value.split(","):
            if not part:
                raise ValueError("invalid page range")
            bounds = part.split("-")
            if len(bounds) > 2 or any(not item.isdigit() for item in bounds):
                raise ValueError("invalid page range")
            numbers = [int(item) for item in bounds]
            if any(number < 1 for number in numbers):
                raise ValueError("pages are one-based")
            if len(numbers) == 2 and numbers[0] > numbers[1]:
                raise ValueError("page range must be ascending")
        return value

    @field_validator("model")
    @classmethod
    def normalize_optional_model(cls, value: str | None) -> str | None:
        if value is None:
            return None
        normalized = value.strip()
        if not normalized:
            raise ValueError("value cannot be blank")
        if any(ord(character) < 0x20 for character in normalized):
            raise ValueError("model cannot contain control characters")
        return normalized

    @field_validator("provider_name")
    @classmethod
    def normalize_optional_provider_name(cls, value: str | None) -> str | None:
        if value is None:
            return None
        normalized = " ".join(value.split())
        if not normalized:
            raise ValueError("value cannot be blank")
        return normalized

    @field_validator("openai_base_url")
    @classmethod
    def normalize_openai_base_url(cls, value: str | None) -> str | None:
        if value is None:
            return None
        return _normalized_openai_base_url(value)

    @field_validator("output_dual")
    @classmethod
    def validate_output_choice(cls, value: bool, info):
        if not value and info.data.get("output_mono") is False:
            raise ValueError("at least one output must be enabled")
        return value

    @model_validator(mode="after")
    def validate_provider_options(self) -> TranslationRequest:
        if self.openai_api_key is not None:
            api_key = self.openai_api_key.get_secret_value().strip()
            if not api_key:
                raise ValueError("OpenAI API key cannot be blank")
            if len(api_key) > 8192:
                raise ValueError("OpenAI API key is too long")
            self.openai_api_key = SecretStr(api_key)
        engine_options = (
            self.model,
            self.thinking_mode,
            self.reasoning_effort,
            self.provider_name,
            self.openai_base_url,
            self.openai_api_key,
        )
        if self.provider == "google":
            if any(option is not None for option in engine_options):
                raise ValueError("Google does not accept configured-provider options")
            return self
        if self.openai_api_key is None:
            raise ValueError("Configured providers require an API key")
        if self.provider == "deepseek":
            if self.openai_base_url is not None:
                raise ValueError("DeepSeek uses its fixed official base URL")
            if self.model is not None and self.model not in DEEPSEEK_MODELS:
                raise ValueError("Unsupported DeepSeek model")
            if self.reasoning_effort is not None and self.thinking_mode != "enabled":
                raise ValueError(
                    "DeepSeek reasoning effort requires explicitly enabled "
                    "thinking mode"
                )
            return self
        if self.model is None:
            raise ValueError("OpenAI-compatible providers require a model")
        if self.openai_base_url is None:
            raise ValueError("OpenAI-compatible providers require a base URL")
        if self.thinking_mode is not None:
            raise ValueError("Thinking mode is only supported by the DeepSeek provider")
        return self


class WorkerState(BaseModel):
    id: str  # noqa: A003 - wire protocol field
    status: Literal["running", "finished", "failed", "cancelled"]


class WorkerControlState(BaseModel):
    status: Literal["draining", "idle", "ready"]
    active_jobs: int


class TerminalJournalError(RuntimeError):
    """A durable worker terminal record failed structural validation."""


@dataclass
class WorkerJob:
    job_id: str
    request: TranslationRequest = field(repr=False)
    events: list[dict] = field(default_factory=list)
    event_offset: int = 0
    terminal_journal_path: Path | None = None
    condition: asyncio.Condition = field(default_factory=asyncio.Condition)
    task: asyncio.Task | None = None
    status: str = "running"
    terminal: bool = False
    cleanup_complete: threading.Event = field(
        default_factory=threading.Event, repr=False
    )
    cancel_watchdog_started: bool = False
    cancel_requested: bool = False

    async def emit(self, event: dict) -> None:
        event = {"job_id": self.job_id, **event}
        if event.get("type") in TERMINAL_EVENTS:
            allowed_fields = {
                "job_id",
                "type",
                "progress",
                "stage",
                "message",
                "files",
            }
            event = {
                key: value for key, value in event.items() if key in allowed_fields
            }
            event_type = event.get("type")
            if event_type == "finished":
                event["stage"] = "翻译完成"
                event["message"] = "文件已整理，可以下载"
            elif event_type == "cancelled":
                event["stage"] = "已取消"
                event["message"] = "翻译进程与临时资源已回收"
            else:
                event["stage"] = "翻译失败"
                event["message"] = _public_error(event.get("message"), self.request)
        async with self.condition:
            event_type = event.get("type")
            if self.terminal:
                return
            if event_type in TERMINAL_EVENTS:
                if PUBLIC_WORKER_MODE and self.terminal_journal_path is None:
                    _fail_stop_worker(
                        self.job_id,
                        RuntimeError("public terminal job has no journal path"),
                        self.request,
                    )
                if self.terminal_journal_path is not None:
                    try:
                        _persist_terminal_event(self.terminal_journal_path, event)
                    except (OSError, TypeError, ValueError) as exc:
                        _fail_stop_worker(self.job_id, exc, self.request)
                        raise UnrecoverableCleanupError(
                            "Could not commit the worker terminal journal"
                        ) from exc
                # A committed terminal supersedes progress history. Keeping
                # only this event makes long retention/recovery inexpensive
                # and guarantees reconnecting consumers see the outcome.
                self.event_offset += len(self.events)
                self.events.clear()
                self.events.append(event)
                self.status = str(event_type)
                self.terminal = True
                # Terminal jobs are retained for status polling, but never
                # retain the per-request provider credential. Doing this at
                # publication also covers tasks cancelled before their
                # coroutine body (and its finally block) is ever polled.
                self.request.openai_api_key = None
                self.cleanup_complete.set()
                # Admission closes only after the sanitized terminal record is
                # durably committed, but before another handler can observe
                # this job as terminal and submit work to the same epoch.
                _retire_container_after_terminal(self.job_id)
            else:
                self.events.append(event)
                overflow = len(self.events) - MAX_JOURNAL_EVENTS
                if overflow > 0:
                    del self.events[:overflow]
                    self.event_offset += overflow
            self.condition.notify_all()


_jobs: dict[str, WorkerJob] = {}
_cancelled_before_start: dict[str, object] = {}
_jobs_lock = asyncio.Lock()
_accepting_jobs = True
_retire_requested = False
_recycle_scheduled = threading.Event()


def _worker_process_has_app_identity() -> bool:
    get_effective_uid = getattr(os, "geteuid", None)
    get_effective_gid = getattr(os, "getegid", None)
    return (
        get_effective_uid is not None
        and get_effective_gid is not None
        and get_effective_uid() == 10001
        and get_effective_gid() == 10001
    )


def _validate_runtime_configuration() -> None:
    raw_max_active_jobs = os.getenv("PDF2ZH_MAX_ACTIVE_JOBS", "1")
    try:
        max_active_jobs = int(raw_max_active_jobs)
    except ValueError as exc:
        raise RuntimeError("PDF2ZH_MAX_ACTIVE_JOBS must be exactly 1") from exc
    if max_active_jobs != 1:
        raise RuntimeError("PDF2ZH_MAX_ACTIVE_JOBS must be exactly 1")
    if PUBLIC_WORKER_MODE:
        configured_boot_id = _CONFIGURED_CONTAINER_BOOT_ID
        try:
            parsed_boot_id = UUID(configured_boot_id)
        except ValueError as exc:
            raise RuntimeError(
                "PDF2ZH_CONTAINER_BOOT_ID must be supplied by the worker supervisor"
            ) from exc
        if str(parsed_boot_id) != configured_boot_id:
            raise RuntimeError(
                "PDF2ZH_CONTAINER_BOOT_ID must be a canonical supervisor UUID"
            )
        if not _worker_process_has_app_identity():
            raise RuntimeError(
                "public worker API must run as unprivileged uid/gid 10001"
            )


def _retire_container_after_terminal(job_id: str) -> None:
    """Close admission and recycle this public worker after one terminal job."""
    global _accepting_jobs
    global _retire_requested
    if not PUBLIC_WORKER_MODE:
        return
    _accepting_jobs = False
    _retire_requested = True
    if _recycle_scheduled.is_set():
        return
    _recycle_scheduled.set()
    logger.info(
        "Job %s committed its terminal journal; retiring worker epoch %s",
        job_id,
        WORKER_EPOCH,
    )
    try:
        exit_timer = threading.Timer(
            WORKER_RECYCLE_DELAY_SECONDS,
            os._exit,
            args=(WORKER_RECYCLE_EXIT_CODE,),
        )
        exit_timer.daemon = True
        exit_timer.start()
    except Exception as exc:
        logger.critical(
            "Could not arm terminal worker recycle (%s); exiting immediately",
            type(exc).__name__,
        )
        os._exit(WORKER_RECYCLE_EXIT_CODE)


@asynccontextmanager
async def lifespan(_: FastAPI):
    global _accepting_jobs
    if not WORKER_TOKEN:
        raise RuntimeError("PDF2ZH_WORKER_TOKEN is required")
    _validate_runtime_configuration()
    DATA_ROOT.mkdir(parents=True, exist_ok=True)
    yield
    async with _jobs_lock:
        _accepting_jobs = False
        active_jobs = [job for job in _jobs.values() if not job.terminal]
    if active_jobs:
        await asyncio.gather(
            *(_cancel_existing_job(job) for job in active_jobs),
            return_exceptions=True,
        )
        tasks = [
            job.task
            for job in active_jobs
            if job.task is not None and not job.task.done()
        ]
    else:
        tasks = []
    if tasks:
        done, pending = await asyncio.wait(tasks, timeout=10)
        logger.info(
            "Worker shutdown cleanup: %s completed, %s pending",
            len(done),
            len(pending),
        )


app = FastAPI(
    title="PDFMathTranslate Python worker",
    description=(
        "Private Google/DeepSeek/OpenAI-compatible BabelDOC bridge. "
        "The Rust service is the public API."
    ),
    version="1.0.0",
    docs_url=None,
    redoc_url=None,
    openapi_url=None,
    lifespan=lifespan,
)


@app.middleware("http")
async def reject_job_body_after_terminal(request: Request, call_next):
    # Route dependencies run after FastAPI has parsed TranslationRequest. Once
    # this epoch has committed a terminal, reject an exact start-job route at
    # the ASGI boundary so a provider credential is never deserialized into the
    # retiring process during the short journal-recovery handoff.
    path_parts = request.url.path.strip("/").split("/")
    if (
        PUBLIC_WORKER_MODE
        and _retire_requested
        and request.method == "POST"
        and len(path_parts) == 3
        and path_parts[:2] == ["internal", "jobs"]
    ):
        return JSONResponse(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            content={"detail": "worker generation is retiring"},
        )
    return await call_next(request)


@app.exception_handler(RequestValidationError)
async def sanitized_request_validation_error(
    _: Request, error: RequestValidationError
) -> JSONResponse:
    # FastAPI's default 422 body includes Pydantic's original `input` value.
    # Configured-provider requests contain credentials, so omit input/context
    # entirely before malformed input can echo a supplied key.
    details = [
        {key: item[key] for key in ("type", "loc", "msg") if key in item}
        for item in error.errors()
    ]
    return JSONResponse(status_code=422, content={"detail": details})


def require_worker_token(
    x_worker_token: Annotated[str | None, Header()] = None,
) -> None:
    if not WORKER_TOKEN or not secrets.compare_digest(
        x_worker_token or "", WORKER_TOKEN
    ):
        raise HTTPException(status_code=status.HTTP_401_UNAUTHORIZED)


WorkerAuth: TypeAlias = Annotated[None, Depends(require_worker_token)]


def _validate_job_id(job_id: str) -> None:
    try:
        parsed_job_id = UUID(job_id)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail="invalid job id") from exc
    if str(parsed_job_id) != job_id:
        raise HTTPException(status_code=400, detail="job id must be canonical")


def _validated_paths(job_id: str, request: TranslationRequest) -> tuple[Path, Path]:
    _validate_job_id(job_id)
    raw_job_root = DATA_ROOT / job_id
    if raw_job_root.is_symlink():
        raise HTTPException(status_code=400, detail="job directory cannot be a symlink")
    job_root = raw_job_root.resolve()
    if job_root.parent != DATA_ROOT:
        raise HTTPException(status_code=400, detail="job directory escapes data root")
    input_path = Path(request.input_path).resolve()
    output_dir = Path(request.output_dir).resolve()
    if input_path != job_root / "input.pdf" or output_dir != job_root / "output":
        raise HTTPException(
            status_code=400, detail="paths do not match the job directory"
        )
    if not input_path.is_file():
        raise HTTPException(status_code=400, detail="input PDF does not exist")
    try:
        with input_path.open("rb") as source:
            if source.read(5) != b"%PDF-":
                raise HTTPException(status_code=400, detail="input is not a PDF")
    except OSError as exc:
        raise HTTPException(status_code=400, detail="input PDF is unreadable") from exc
    try:
        output_dir.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise HTTPException(
            status_code=400, detail="output directory is unusable"
        ) from exc
    return input_path, output_dir


def _terminal_journal_path(job_id: str) -> Path:
    _validate_job_id(job_id)
    raw_job_root = DATA_ROOT / job_id
    if raw_job_root.is_symlink():
        raise HTTPException(status_code=400, detail="job directory cannot be a symlink")
    job_root = raw_job_root.resolve()
    if job_root.parent != DATA_ROOT:
        raise HTTPException(status_code=400, detail="job directory escapes data root")
    raw_output_dir = job_root / "output"
    if raw_output_dir.is_symlink() or raw_output_dir.resolve() != raw_output_dir:
        raise HTTPException(status_code=400, detail="output directory is invalid")
    return raw_output_dir / TERMINAL_JOURNAL_FILENAME


def _fsync_directory(directory: Path) -> None:
    if os.name != "posix":
        return
    descriptor = os.open(directory, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _fsync_file(path: Path) -> None:
    with path.open("rb") as source:
        os.fsync(source.fileno())


def _persist_terminal_event(path: Path, event: dict) -> None:
    if (
        not path.is_absolute()
        or path.name not in {TERMINAL_JOURNAL_FILENAME, TERMINAL_RECOVERY_FILENAME}
        or path.parent.is_symlink()
        or path.parent.resolve() != path.parent
    ):
        raise ValueError("terminal journal path is invalid")
    encoded = json.dumps(
        event,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    if len(encoded) > MAX_TERMINAL_JOURNAL_BYTES:
        raise ValueError("terminal journal is too large")
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{uuid4().hex}.tmp")
    try:
        with temporary.open("xb") as destination:
            destination.write(encoded)
            destination.flush()
            os.fsync(destination.fileno())
        temporary.replace(path)
        _fsync_directory(path.parent)
    finally:
        temporary.unlink(missing_ok=True)


def _read_terminal_event(path: Path, job_id: str) -> dict:
    if path.is_symlink():
        raise TerminalJournalError("terminal journal is a symlink")
    if not path.is_file():
        raise TerminalJournalError("terminal journal is not a regular file")
    try:
        with path.open("rb") as source:
            payload = source.read(MAX_TERMINAL_JOURNAL_BYTES + 1)
        if len(payload) > MAX_TERMINAL_JOURNAL_BYTES:
            raise TerminalJournalError("terminal journal is too large")
        event = json.loads(payload.decode("utf-8"))
    except (OSError, UnicodeDecodeError, ValueError) as exc:
        raise TerminalJournalError("terminal journal is unreadable") from exc
    if (
        not isinstance(event, dict)
        or event.get("job_id") != job_id
        or event.get("type") not in TERMINAL_EVENTS
    ):
        raise TerminalJournalError("terminal journal has invalid identity or type")
    event_type = event["type"]
    if event_type == "finished":
        files = event.get("files")
        if (
            not isinstance(files, dict)
            or not all(
                isinstance(files.get(name), bool)
                for name in ("mono", "dual", "glossary")
            )
            or not (files["mono"] or files["dual"])
        ):
            raise TerminalJournalError("finished journal has invalid outputs")
    elif not isinstance(event.get("message"), str):
        raise TerminalJournalError("terminal journal has no public message")
    return event


def _corrupt_terminal_event(job_id: str) -> dict:
    return {
        "job_id": job_id,
        "type": "failed",
        "stage": "翻译失败",
        "message": "任务终态记录损坏，已安全终止，请重新提交",
    }


def _repair_terminal_journal(
    job_id: str,
    path: Path,
    error: BaseException,
) -> dict:
    logger.error("Quarantining terminal journal for job %s: %s", job_id, error)
    recovery_path = path.with_name(TERMINAL_RECOVERY_FILENAME)
    failed_event = _corrupt_terminal_event(job_id)
    try:
        # The recovery marker is durable before the corrupt path moves. If the
        # process dies between renames, the next epoch promotes this marker.
        _persist_terminal_event(recovery_path, failed_event)
        quarantine = path.with_name(f".worker-terminal.corrupt.{uuid4().hex}")
        path.replace(quarantine)
        _fsync_directory(path.parent)
        recovery_path.replace(path)
        _fsync_directory(path.parent)
    except (OSError, TypeError, ValueError) as exc:
        _fail_stop_worker(job_id, exc)
        raise UnrecoverableCleanupError(
            "Could not quarantine the corrupt terminal journal"
        ) from exc
    return failed_event


def _load_terminal_event(job_id: str) -> dict | None:
    """Load or repair a journal after confirming no registry job owns the ID."""
    path = _terminal_journal_path(job_id)
    recovery_path = path.with_name(TERMINAL_RECOVERY_FILENAME)
    path_exists = path.exists() or path.is_symlink()
    recovery_exists = recovery_path.exists() or recovery_path.is_symlink()
    if not path_exists:
        if not recovery_exists:
            return None
        try:
            event = _read_terminal_event(recovery_path, job_id)
        except TerminalJournalError as error:
            logger.error(
                "Quarantining terminal recovery marker for job %s: %s",
                job_id,
                error,
            )
            event = _corrupt_terminal_event(job_id)
            try:
                # The official path is absent, so commit the replacement there
                # before moving the corrupt recovery marker. This keeps a
                # durable terminal at every crash boundary.
                _persist_terminal_event(path, event)
                quarantine = recovery_path.with_name(
                    f".worker-terminal.recovery.corrupt.{uuid4().hex}"
                )
                recovery_path.replace(quarantine)
                _fsync_directory(path.parent)
            except (OSError, TypeError, ValueError) as exc:
                _fail_stop_worker(job_id, exc)
                raise UnrecoverableCleanupError(
                    "Could not repair the terminal recovery marker"
                ) from exc
            return event
        try:
            recovery_path.replace(path)
            _fsync_directory(path.parent)
        except OSError as exc:
            _fail_stop_worker(job_id, exc)
            raise UnrecoverableCleanupError(
                "Could not promote the terminal recovery marker"
            ) from exc
        return event
    try:
        return _read_terminal_event(path, job_id)
    except TerminalJournalError as exc:
        return _repair_terminal_journal(job_id, path, exc)


def _build_settings(request: TranslationRequest, output_dir: Path) -> SettingsModel:
    if request.provider == "deepseek":
        # DeepSeek's current API accepts high/max. Its compatibility mapping
        # treats low as high, so normalize before constructing OpenAI options.
        deepseek_reasoning_effort = (
            "high" if request.reasoning_effort == "low" else request.reasoning_effort
        )
        translate_engine_settings = DeepSeekSettings(
            deepseek_model=request.model or "deepseek-v4-flash",
            deepseek_api_key=_request_api_key(request),
            deepseek_thinking_mode=request.thinking_mode,
            deepseek_reasoning_effort=deepseek_reasoning_effort,
        )
    elif request.provider == "openai_compatible":
        # Re-resolve the hostname at job start. This narrows DNS-rebinding
        # windows and makes the worker the final application-level authority on
        # outbound URLs. DNS can still change between this check and HTTPX's
        # connection; an egress firewall/proxy is required for an atomic network
        # boundary.
        base_url = _normalized_openai_base_url(request.openai_base_url or "")
        translate_engine_settings = OpenAICompatibleSettings(
            openai_compatible_model=request.model or "",
            openai_compatible_base_url=base_url,
            openai_compatible_api_key=_request_api_key(request),
            openai_compatible_reasoning_effort=request.reasoning_effort,
            openai_compatible_send_reasoning_effort=(
                request.reasoning_effort is not None
            ),
        )
    else:
        translate_engine_settings = GoogleSettings()
    settings = SettingsModel(
        translation=TranslationSettings(
            lang_in=request.source_lang,
            lang_out=request.target_lang,
            output=str(output_dir),
            # Public jobs can contain confidential document text. Do not put
            # source/translated fragments into the process-wide SQLite cache.
            ignore_cache=True,
            no_auto_extract_glossary=True,
            qps=4,
            pool_max_workers=4,
        ),
        pdf=PDFSettings(
            pages=request.pages,
            no_mono=not request.output_mono,
            no_dual=not request.output_dual,
            watermark_output_mode="no_watermark",
        ),
        translate_engine_settings=translate_engine_settings,
        term_extraction_engine_settings=None,
        report_interval=0.2,
    )
    settings.validate_settings()
    return settings


def _request_api_key(request: TranslationRequest) -> str:
    if request.openai_api_key is None:
        raise ValueError("Configured provider API key is unavailable")
    return request.openai_api_key.get_secret_value()


def _copy_result(source: object, destination: Path) -> bool:
    if source is None:
        return False
    source_path = Path(str(source)).resolve()
    if not source_path.is_file():
        return False
    destination.parent.mkdir(parents=True, exist_ok=True)
    if source_path != destination.resolve():
        temporary = destination.with_name(f".{destination.name}.{uuid4().hex}.tmp")
        try:
            shutil.copy2(source_path, temporary)
            _fsync_file(temporary)
            temporary.replace(destination)
            _fsync_directory(destination.parent)
        finally:
            temporary.unlink(missing_ok=True)
    else:
        _fsync_file(destination)
        _fsync_directory(destination.parent)
    return True


def _clear_stable_results(output_dir: Path) -> None:
    for filename in ("mono.pdf", "dual.pdf", "glossary.csv"):
        path = output_dir / filename
        try:
            if path.is_symlink() or path.is_file():
                path.unlink()
            elif path.exists():
                raise HTTPException(
                    status_code=400,
                    detail=f"output path {filename} is not a regular file",
                )
        except OSError as exc:
            raise HTTPException(
                status_code=400, detail=f"output path {filename} is unusable"
            ) from exc


def _materialize_results(result: object, output_dir: Path) -> dict[str, bool]:
    mono_source = getattr(result, "no_watermark_mono_pdf_path", None) or getattr(
        result, "mono_pdf_path", None
    )
    dual_source = getattr(result, "no_watermark_dual_pdf_path", None) or getattr(
        result, "dual_pdf_path", None
    )
    return {
        "mono": _copy_result(mono_source, output_dir / "mono.pdf"),
        "dual": _copy_result(dual_source, output_dir / "dual.pdf"),
        "glossary": _copy_result(
            getattr(result, "auto_extracted_glossary_path", None),
            output_dir / "glossary.csv",
        ),
    }


def _progress_event(event: dict) -> dict:
    progress = float(event.get("overall_progress", 0.0))
    progress = max(0.0, min(100.0, progress))
    stage = " ".join(str(event.get("stage", "正在翻译")).split())[:120]
    stage = stage or "正在翻译"
    current = event.get("stage_current")
    total = event.get("stage_total")
    part = event.get("part_index")
    parts = event.get("total_parts")
    details = []
    if part is not None and parts is not None:
        details.append(f"分卷 {part}/{parts}")
    if current is not None and total is not None:
        details.append(f"{current}/{total}")
    return {
        "type": "progress",
        "progress": progress,
        "stage": stage,
        "message": " · ".join(details) or stage,
    }


async def _run_translation(job: WorkerJob, input_path: Path, output_dir: Path) -> None:
    try:
        # DNS lookup runs off the event loop and has a hard bound, so a slow
        # administrator-configured resolver cannot stall cancellation or the
        # worker health endpoint. Only the hostname enters that helper thread;
        # the provider credential remains on this task.
        await _validate_openai_endpoint_for_job(job.request)
        settings = _build_settings(job.request, output_dir)
        terminal_event = None
        provider_name = {
            "google": "Google",
            "deepseek": "DeepSeek",
            "openai_compatible": "OpenAI 兼容",
        }[job.request.provider]
        provider_name = _safe_public_label(
            job.request.provider_name or provider_name,
        )
        await job.emit(
            {
                "type": "progress",
                "progress": 0.0,
                "stage": "准备文档",
                "message": f"已进入 {provider_name} 翻译管线",
            }
        )
        async with aclosing(do_translate_async_stream(settings, input_path)) as stream:
            async for event in stream:
                event_type = event.get("type")
                if event_type in {"progress_start", "progress_update", "progress_end"}:
                    await job.emit(_progress_event(event))
                elif event_type == "finish":
                    files = _materialize_results(event["translate_result"], output_dir)
                    if not files["mono"] and not files["dual"]:
                        raise RuntimeError("translation finished without a PDF result")
                    terminal_event = {
                        "type": "finished",
                        "progress": 100.0,
                        "stage": "翻译完成",
                        "message": "文件已整理，可以下载",
                        "files": files,
                    }
                    break
                elif event_type == "error":
                    if event.get("error_type") == "UnrecoverableCleanupError":
                        cleanup_error = UnrecoverableCleanupError(
                            str(event.get("error") or "cleanup was not confirmed")
                        )
                        _fail_stop_worker(job.job_id, cleanup_error, job.request)
                        raise cleanup_error
                    terminal_event = {
                        "type": "failed",
                        "stage": "翻译失败",
                        "message": event.get("error"),
                    }
                    break
        if terminal_event is None:
            terminal_event = {
                "type": "failed",
                "stage": "翻译失败",
                "message": "翻译进程提前结束，未返回结果",
            }
        # Terminal publication is deliberately outside `aclosing`: Rust may
        # release its permit only after every Python generator cleanup is done.
        await job.emit(terminal_event)
    except UnrecoverableCleanupError as exc:
        _fail_stop_worker(job.job_id, exc, job.request)
    except asyncio.CancelledError:
        logger.info("Translation job %s was cancelled after cleanup", job.job_id)
        await job.emit(
            {
                "type": "cancelled",
                "stage": "已取消",
                "message": "翻译进程与临时资源已回收",
            }
        )
    except Exception as exc:
        logger.error(
            "Translation job %s failed (%s)",
            job.job_id,
            type(exc).__name__,
        )
        if not job.terminal:
            await job.emit(
                {
                    "type": "failed",
                    "stage": "翻译失败",
                    "message": exc,
                }
            )
    finally:
        # Terminal jobs remain queryable for several hours, but their provider
        # credential must not remain reachable through that retained object.
        job.request.openai_api_key = None
        asyncio.create_task(_expire_job(job.job_id, job))


async def _expire_job(job_id: str, expected_job: WorkerJob) -> None:
    await asyncio.sleep(JOB_RETENTION_SECONDS)
    async with _jobs_lock:
        job = _jobs.get(job_id)
        if job is expected_job and job.terminal:
            _jobs.pop(job_id, None)


def _remember_cancel_tombstone_locked(job_id: str) -> None:
    marker = object()
    _cancelled_before_start[job_id] = marker
    raw_job_root = DATA_ROOT / job_id
    if raw_job_root.is_dir() and not raw_job_root.is_symlink():
        event = {
            "job_id": job_id,
            "type": "cancelled",
            "stage": "已取消",
            "message": "任务在启动前已取消，未创建翻译进程",
        }
        try:
            _persist_terminal_event(_terminal_journal_path(job_id), event)
        except (HTTPException, OSError, TypeError, ValueError) as exc:
            _fail_stop_worker(job_id, exc)
            raise UnrecoverableCleanupError(
                "Could not commit the cancellation tombstone"
            ) from exc
    asyncio.create_task(_expire_cancel_tombstone(job_id, marker))


async def _expire_cancel_tombstone(job_id: str, expected_marker: object) -> None:
    await asyncio.sleep(JOB_RETENTION_SECONDS)
    async with _jobs_lock:
        if _cancelled_before_start.get(job_id) is expected_marker:
            _cancelled_before_start.pop(job_id, None)


def _public_error(
    error: object,
    request: TranslationRequest | None = None,
) -> str:
    del request  # Credentials and arbitrary upstream text are never reflected.
    message = " ".join(str(error or "").split())
    if message in PUBLIC_FAILURE_MESSAGES:
        return message
    normalized = message.casefold()
    if any(
        marker in normalized
        for marker in (
            "authentication",
            "unauthorized",
            "authorization",
            "api_key",
            "api key",
            "invalid api key",
            "incorrect api key",
            "status 401",
            "status code: 401",
            "status 403",
            "status code: 403",
        )
    ):
        return "翻译渠道认证失败，请联系管理员检查密钥"
    if any(
        marker in normalized
        for marker in (
            "rate limit",
            "too many requests",
            "quota",
            "billing",
            "status 429",
            "status code: 429",
        )
    ):
        return "翻译渠道请求过于频繁或额度不足，请稍后重试"
    if any(marker in normalized for marker in ("timeout", "timed out")):
        return "翻译渠道响应超时，请稍后重试"
    if any(
        marker in normalized
        for marker in (
            "connection",
            "connect error",
            "dns",
            "name resolution",
            "network is unreachable",
        )
    ):
        return "无法连接翻译渠道，请稍后重试"
    if any(
        marker in normalized
        for marker in (
            "bad request",
            "invalid_request",
            "model not found",
            "status 400",
            "status code: 400",
            "status 404",
            "status code: 404",
            "status 422",
            "status code: 422",
        )
    ):
        return "翻译渠道拒绝了请求，请联系管理员检查模型配置"
    return "翻译服务暂时无法完成任务，请稍后重试"


def _safe_public_label(value: object) -> str:
    label = " ".join(str(value or "").split())
    return label[:120] or "已配置渠道"


def _fail_stop_worker(
    job_id: str,
    error: BaseException,
    request: TranslationRequest | None = None,
) -> None:
    del request
    logger.critical(
        "Job %s cleanup could not be proven complete; failing the container (%s)",
        job_id,
        type(error).__name__,
    )
    os._exit(70)


def _arm_cancel_watchdog(job: WorkerJob) -> None:
    if job.cancel_watchdog_started or job.cleanup_complete.is_set():
        return
    job.cancel_watchdog_started = True

    def enforce_cleanup_deadline() -> None:
        if job.cleanup_complete.wait(CANCEL_HARD_TIMEOUT_SECONDS):
            return
        logger.critical(
            "Translation job %s did not clean up within %s seconds; "
            "terminating the worker process for supervisor recovery",
            job.job_id,
            CANCEL_HARD_TIMEOUT_SECONDS,
        )
        os._exit(70)

    threading.Thread(
        target=enforce_cleanup_deadline,
        name=f"cancel-watchdog-{job.job_id}",
        daemon=True,
    ).start()


@app.get("/internal/health")
async def health(_: WorkerAuth) -> dict:
    active = sum(1 for job in _jobs.values() if not job.terminal)
    return {
        "status": "ok" if _accepting_jobs else "draining",
        "provider": "google",
        "providers": ["google", "deepseek", "openai_compatible"],
        "active_jobs": active,
        "accepting_jobs": _accepting_jobs,
        "epoch": WORKER_EPOCH,
        "container_boot_id": CONTAINER_BOOT_ID,
        "max_active_jobs": 1,
    }


@app.post("/internal/fail-stop", status_code=202)
async def fail_stop_worker(
    _: WorkerAuth,
) -> dict:
    logger.critical("Authenticated fail-stop requested by the Rust coordinator")
    exit_timer = threading.Timer(0.05, os._exit, args=(70,))
    exit_timer.daemon = True
    exit_timer.start()
    return {
        "status": "terminating",
        "epoch": WORKER_EPOCH,
        "container_boot_id": CONTAINER_BOOT_ID,
    }


@app.post("/internal/jobs/{job_id}", response_model=WorkerState, status_code=202)
async def start_job(
    job_id: str,
    request: TranslationRequest,
    _: WorkerAuth,
) -> WorkerState:
    _validate_job_id(job_id)
    async with _jobs_lock:
        existing = _jobs.get(job_id)
        if existing is not None:
            return WorkerState(id=job_id, status=existing.status)
        # The journal is the durable idempotency record across worker epochs.
        # It is checked before input validation because a completed task may
        # already have had its input removed by the coordinator.
        persisted_terminal = _load_terminal_event(job_id)
        if persisted_terminal is not None:
            return WorkerState(
                id=job_id,
                status=str(persisted_terminal["type"]),
            )
        if job_id in _cancelled_before_start:
            return WorkerState(id=job_id, status="cancelled")
        if not _accepting_jobs:
            _remember_cancel_tombstone_locked(job_id)
            return WorkerState(id=job_id, status="cancelled")
        if any(not job.terminal for job in _jobs.values()):
            raise HTTPException(
                status_code=status.HTTP_409_CONFLICT,
                detail="worker already has an active job",
            )
        input_path, output_dir = _validated_paths(job_id, request)
        _clear_stable_results(output_dir)
        job = WorkerJob(
            job_id=job_id,
            request=request,
            terminal_journal_path=output_dir / TERMINAL_JOURNAL_FILENAME,
        )
        _jobs[job_id] = job
        job.task = asyncio.create_task(_run_translation(job, input_path, output_dir))
    return WorkerState(id=job_id, status="running")


@app.get("/internal/jobs/{job_id}/events")
async def job_events(
    job_id: str,
    _: WorkerAuth,
) -> StreamingResponse:
    _validate_job_id(job_id)
    async with _jobs_lock:
        job = _jobs.get(job_id)
    if job is None:
        persisted_terminal = _load_terminal_event(job_id)
        if persisted_terminal is None:
            raise HTTPException(status_code=404, detail="job not found")

        async def persisted_stream():
            yield (
                json.dumps(
                    persisted_terminal,
                    ensure_ascii=False,
                    separators=(",", ":"),
                )
                + "\n"
            )

        return StreamingResponse(persisted_stream(), media_type="application/x-ndjson")

    async def stream():
        cursor = 0
        while True:
            async with job.condition:
                if cursor < job.event_offset:
                    cursor = job.event_offset
                while cursor >= job.event_offset + len(job.events) and not job.terminal:
                    await job.condition.wait()
                    if cursor < job.event_offset:
                        cursor = job.event_offset
                start = max(0, cursor - job.event_offset)
                batch = job.events[start:]
                cursor = job.event_offset + len(job.events)
                terminal = job.terminal
            for event in batch:
                yield (
                    json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n"
                )
            if terminal and cursor >= job.event_offset + len(job.events):
                return

    return StreamingResponse(stream(), media_type="application/x-ndjson")


@app.get("/internal/jobs/{job_id}/terminal")
async def job_terminal(
    job_id: str,
    _: WorkerAuth,
) -> dict:
    _validate_job_id(job_id)
    async with _jobs_lock:
        job = _jobs.get(job_id)
    if job is None:
        persisted_terminal = _load_terminal_event(job_id)
        if persisted_terminal is None:
            raise HTTPException(status_code=404, detail="job not found")
        return persisted_terminal
    async with job.condition:
        if not job.terminal:
            raise HTTPException(status_code=409, detail="job is not terminal")
        for event in reversed(job.events):
            if event.get("type") in TERMINAL_EVENTS:
                return dict(event)
    logger.critical("Terminal job %s has no terminal event", job_id)
    raise HTTPException(status_code=500, detail="terminal event is unavailable")


@app.post("/internal/jobs/{job_id}/cancel", response_model=WorkerState)
async def cancel_job(
    job_id: str,
    _: WorkerAuth,
) -> WorkerState:
    _validate_job_id(job_id)
    async with _jobs_lock:
        job = _jobs.get(job_id)
        if job is None:
            persisted_terminal = _load_terminal_event(job_id)
            if persisted_terminal is not None:
                return WorkerState(
                    id=job_id,
                    status=str(persisted_terminal["type"]),
                )
            if job_id not in _cancelled_before_start:
                _remember_cancel_tombstone_locked(job_id)
    if job is None:
        return WorkerState(id=job_id, status="cancelled")
    return await _cancel_existing_job(job)


async def _cancel_existing_job(job: WorkerJob) -> WorkerState:
    task = job.task
    async with job.condition:
        first_cancel_request = not job.cancel_requested
        job.cancel_requested = True
    if first_cancel_request and task is not None and not task.done():
        _arm_cancel_watchdog(job)
        task.cancel()
    if task is not None and not task.done():
        try:
            await asyncio.wait_for(asyncio.shield(task), timeout=30)
        except (asyncio.CancelledError, asyncio.TimeoutError):
            pass
    if task is not None and task.done() and not job.terminal:
        await job.emit(
            {
                "type": "cancelled",
                "stage": "已取消",
                "message": "任务在启动前已取消，未创建翻译进程",
            }
        )
        asyncio.create_task(_expire_job(job.job_id, job))
    return WorkerState(id=job.job_id, status=job.status)


@app.post("/internal/drain", response_model=WorkerControlState)
async def drain_worker(
    _: WorkerAuth,
) -> WorkerControlState:
    global _accepting_jobs
    async with _jobs_lock:
        _accepting_jobs = False
        active_jobs = [job for job in _jobs.values() if not job.terminal]
    if active_jobs:
        await asyncio.gather(*(_cancel_existing_job(job) for job in active_jobs))
    remaining = sum(1 for job in _jobs.values() if not job.terminal)
    return WorkerControlState(
        status="idle" if remaining == 0 else "draining",
        active_jobs=remaining,
    )


@app.post("/internal/resume", response_model=WorkerControlState)
async def resume_worker(
    _: WorkerAuth,
) -> WorkerControlState:
    global _accepting_jobs
    async with _jobs_lock:
        active = sum(1 for job in _jobs.values() if not job.terminal)
        if active or _retire_requested:
            return WorkerControlState(status="draining", active_jobs=active)
        _accepting_jobs = True
    return WorkerControlState(status="ready", active_jobs=0)
