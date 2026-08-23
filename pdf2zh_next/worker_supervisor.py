from __future__ import annotations

import json
import logging
import math
import os
import signal
import subprocess  # noqa: S404 - fixed interpreter/module invocation only
import sys
import threading
import time
import urllib.request
from pathlib import Path
from uuid import uuid4

logging.basicConfig(
    level=os.getenv("PDF2ZH_SUPERVISOR_LOG_LEVEL", "INFO"),
    format="%(asctime)s %(levelname)s worker-supervisor %(message)s",
)
logger = logging.getLogger("pdf2zh.worker_supervisor")

WORKER_TOKEN = os.getenv("PDF2ZH_WORKER_TOKEN", "")
PUBLIC_WORKER_MODE = os.getenv("PDF2ZH_PUBLIC_WORKER_MODE", "") == "1"
CONTAINER_BOOT_ID = str(uuid4())
WORKER_RECYCLE_EXIT_CODE = 75
APP_UID = 10001
APP_GID = 10001
# Linux capability numbers: KILL=5, SETGID=6, SETUID=7.
SUPERVISOR_CAPABILITY_MASK = (1 << 5) | (1 << 6) | (1 << 7)
HEALTH_URL = "http://127.0.0.1:8000/internal/health"
CHECK_INTERVAL_SECONDS = max(
    1.0, float(os.getenv("PDF2ZH_SUPERVISOR_INTERVAL_SECONDS", "5"))
)
HEALTH_TIMEOUT_SECONDS = max(
    0.5, float(os.getenv("PDF2ZH_SUPERVISOR_HEALTH_TIMEOUT_SECONDS", "2"))
)
STARTUP_TIMEOUT_SECONDS = max(
    10.0, float(os.getenv("PDF2ZH_SUPERVISOR_STARTUP_TIMEOUT_SECONDS", "90"))
)
FAILURE_LIMIT = max(2, int(os.getenv("PDF2ZH_SUPERVISOR_FAILURE_LIMIT", "18")))
CHILD_STOP_TIMEOUT_SECONDS = max(
    5.0, float(os.getenv("PDF2ZH_SUPERVISOR_CHILD_STOP_SECONDS", "45"))
)
_configured_min_generation_seconds = float(
    os.getenv("PDF2ZH_SUPERVISOR_MIN_GENERATION_SECONDS", "11")
)
if not math.isfinite(_configured_min_generation_seconds):
    raise ValueError("PDF2ZH_SUPERVISOR_MIN_GENERATION_SECONDS must be finite")
MIN_GENERATION_SECONDS = max(11.0, _configured_min_generation_seconds)


def _healthy() -> bool:
    request = urllib.request.Request(
        HEALTH_URL,
        headers={"x-worker-token": WORKER_TOKEN},
    )
    try:
        with urllib.request.urlopen(  # noqa: S310 - constant loopback HTTP URL
            request, timeout=HEALTH_TIMEOUT_SECONDS
        ) as response:
            payload = json.load(response)
        return (
            payload.get("status") == "ok"
            and payload.get("provider") == "google"
            and payload.get("max_active_jobs") == 1
            and payload.get("container_boot_id") == CONTAINER_BOOT_ID
            and isinstance(payload.get("epoch"), str)
            and bool(payload["epoch"])
        )
    except Exception:
        return False


def _signal_process_group(process: subprocess.Popen, sig: signal.Signals) -> None:
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass


def _supervisor_is_root() -> bool:
    get_effective_uid = getattr(os, "geteuid", None)
    if get_effective_uid is None or get_effective_uid() != 0:
        return False
    try:
        status_lines = Path("/proc/self/status").read_text().splitlines()
        fields = {
            line.split(":", 1)[0]: line.split(":", 1)[1].strip()
            for line in status_lines
            if ":" in line
        }
        return (
            int(fields["CapEff"], 16) == SUPERVISOR_CAPABILITY_MASK
            and int(fields["CapPrm"], 16) == SUPERVISOR_CAPABILITY_MASK
            and int(fields["CapBnd"], 16) == SUPERVISOR_CAPABILITY_MASK
            and int(fields["CapInh"], 16) == 0
            and int(fields.get("CapAmb", "0"), 16) == 0
            and int(fields["NoNewPrivs"]) == 1
        )
    except (KeyError, OSError, ValueError):
        return False


def _child_has_unprivileged_identity(process: subprocess.Popen) -> bool:
    try:
        status_lines = Path(f"/proc/{process.pid}/status").read_text().splitlines()
    except OSError:
        return False
    fields = {
        line.split(":", 1)[0]: line.split(":", 1)[1].strip()
        for line in status_lines
        if ":" in line
    }
    try:
        uids = {int(value) for value in fields["Uid"].split()}
        gids = {int(value) for value in fields["Gid"].split()}
        supplementary_gids = {int(value) for value in fields.get("Groups", "").split()}
        effective_caps = int(fields["CapEff"], 16)
        permitted_caps = int(fields["CapPrm"], 16)
        inherited_caps = int(fields["CapInh"], 16)
        ambient_caps = int(fields.get("CapAmb", "0"), 16)
        no_new_privileges = int(fields["NoNewPrivs"])
    except (KeyError, ValueError):
        return False
    return (
        uids == {APP_UID}
        and gids == {APP_GID}
        and supplementary_gids.issubset({APP_GID})
        and effective_caps == 0
        and permitted_caps == 0
        and inherited_caps == 0
        and ambient_caps == 0
        and no_new_privileges == 1
    )


def _child_is_stopped(process: subprocess.Popen) -> bool:
    try:
        state = Path(f"/proc/{process.pid}/stat").read_text().split()[2]
    except (OSError, IndexError):
        return False
    return state in {"T", "t"}


def _stop_child(process: subprocess.Popen) -> None:
    _signal_process_group(process, signal.SIGTERM)
    try:
        process.wait(timeout=CHILD_STOP_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        logger.error("worker child ignored SIGTERM; killing its process group")
    else:
        # The Uvicorn leader can exit before a BabelDOC grandchild. Always
        # close its process-group boundary before this supervisor exits.
        time.sleep(0.5)
    _signal_process_group(process, signal.SIGKILL)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        logger.critical("worker process group could not be reaped")


def _kill_child_now(process: subprocess.Popen) -> None:
    _signal_process_group(process, signal.SIGKILL)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        logger.critical("worker process group could not be killed or reaped")


def _wait_for_minimum_generation_lifetime(
    generation_started_at: float,
    stopping: threading.Event,
) -> bool:
    elapsed = max(0.0, time.monotonic() - generation_started_at)
    remaining = max(0.0, MIN_GENERATION_SECONDS - elapsed)
    if remaining == 0:
        return True
    logger.info(
        "terminal cleanup is complete; holding only the supervisor for %.3f "
        "seconds to reset Docker restart backoff",
        remaining,
    )
    return not stopping.wait(remaining)


def _spawn_child() -> subprocess.Popen:
    command = [
        sys.executable,
        "-m",
        "uvicorn",
        "pdf2zh_next.worker_api:app",
        "--host",
        "0.0.0.0",  # noqa: S104 - intentionally exposed only in Docker network
        "--port",
        "8000",
        "--workers",
        "1",
        "--no-access-log",
    ]
    child_environment = os.environ.copy()
    # This value is deliberately generated by the supervisor, not accepted
    # from Compose. A fresh value therefore proves that Docker started a new
    # container init/supervisor generation rather than merely respawning the
    # Uvicorn child in the old cgroup.
    child_environment["PDF2ZH_CONTAINER_BOOT_ID"] = CONTAINER_BOOT_ID
    # Do not place the coordinator token in the Uvicorn process's initial
    # environment. Translation subprocesses share UID 10001; on hosts with a
    # permissive ptrace policy they could otherwise read it through /proc. A
    # one-use inherited pipe is consumed and closed during worker_api import.
    child_environment.pop("PDF2ZH_WORKER_TOKEN", None)
    token_read_fd, token_write_fd = os.pipe()
    try:
        encoded_token = WORKER_TOKEN.encode()
        if len(encoded_token) > 4096:
            raise ValueError("PDF2ZH_WORKER_TOKEN is too long")
        os.write(token_write_fd, encoded_token)
        os.close(token_write_fd)
        token_write_fd = -1
        child_environment["PDF2ZH_WORKER_TOKEN_FD"] = str(token_read_fd)
        return subprocess.Popen(
            command,
            env=child_environment,
            extra_groups=(),
            group=APP_GID,
            pass_fds=(token_read_fd,),
            start_new_session=True,
            umask=0o077,
            user=APP_UID,
        )
    finally:
        if token_write_fd >= 0:
            os.close(token_write_fd)
        os.close(token_read_fd)


def main() -> int:
    generation_started_at = time.monotonic()
    if not WORKER_TOKEN:
        logger.critical("PDF2ZH_WORKER_TOKEN is required")
        return 64
    if not PUBLIC_WORKER_MODE:
        logger.critical("PDF2ZH_PUBLIC_WORKER_MODE=1 is required")
        return 64
    if not _supervisor_is_root():
        logger.critical(
            "worker supervisor requires root with only KILL/SETGID/SETUID "
            "and no-new-privileges"
        )
        return 64
    stopping = threading.Event()

    def request_stop(signum: int, _frame: object) -> None:
        logger.info("received signal %s; stopping worker", signum)
        stopping.set()

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)

    try:
        child = _spawn_child()
    except Exception as exc:
        logger.critical(
            "could not spawn unprivileged worker child (%s)", type(exc).__name__
        )
        return 70
    logger.info("started worker child pid=%s", child.pid)
    startup_deadline = time.monotonic() + STARTUP_TIMEOUT_SECONDS
    ready = False
    failures = 0
    failure_reason = "worker child exited unexpectedly"

    while not stopping.is_set() and child.poll() is None:
        if not _child_has_unprivileged_identity(child):
            failure_reason = "worker child privilege-drop contract was violated"
            logger.error("%s; failing the container", failure_reason)
            _kill_child_now(child)
            break
        if _child_is_stopped(child):
            failure_reason = "worker child was stopped by an unexpected signal"
            logger.error("%s; failing the container", failure_reason)
            _kill_child_now(child)
            break
        if _healthy():
            if not ready:
                logger.info("worker child is healthy")
            ready = True
            failures = 0
        elif not ready:
            if time.monotonic() >= startup_deadline:
                failure_reason = "worker did not become healthy before startup timeout"
                logger.error("%s; failing the container", failure_reason)
                _kill_child_now(child)
                break
        else:
            failures += 1
            if failures >= FAILURE_LIMIT:
                failure_reason = f"worker health failed {failures} consecutive checks"
                logger.error("%s; failing the container", failure_reason)
                _kill_child_now(child)
                break
        # Wait on the child itself rather than sleeping. A terminal journal
        # exit must collapse the old container cgroup immediately; health
        # polling cadence must not add a multi-second escape window.
        try:
            child.wait(timeout=CHECK_INTERVAL_SECONDS)
        except subprocess.TimeoutExpired:
            pass

    if stopping.is_set():
        _stop_child(child)
        logger.info("worker supervisor stopped")
        return 0

    return_code = child.poll()
    _signal_process_group(child, signal.SIGKILL)
    if return_code == WORKER_RECYCLE_EXIT_CODE:
        logger.info(
            "worker committed its terminal journal and exited its process group"
        )
        if not _wait_for_minimum_generation_lifetime(
            generation_started_at,
            stopping,
        ):
            logger.info("container stop requested during terminal recycle cooldown")
            return 0
        return WORKER_RECYCLE_EXIT_CODE
    logger.critical(
        "%s (child return code %s); exiting so Docker destroys the old cgroup",
        failure_reason,
        return_code,
    )
    return 70


if __name__ == "__main__":
    raise SystemExit(main())
