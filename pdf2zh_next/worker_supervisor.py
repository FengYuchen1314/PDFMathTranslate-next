from __future__ import annotations

import json
import logging
import os
import signal
import subprocess  # noqa: S404 - fixed interpreter/module invocation only
import sys
import threading
import time
import urllib.request

logging.basicConfig(
    level=os.getenv("PDF2ZH_SUPERVISOR_LOG_LEVEL", "INFO"),
    format="%(asctime)s %(levelname)s worker-supervisor %(message)s",
)
logger = logging.getLogger("pdf2zh.worker_supervisor")

WORKER_TOKEN = os.getenv("PDF2ZH_WORKER_TOKEN", "")
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
        return payload.get("status") == "ok" and payload.get("provider") == "google"
    except Exception:
        return False


def _signal_process_group(process: subprocess.Popen, sig: signal.Signals) -> None:
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass


def _stop_child(process: subprocess.Popen) -> None:
    _signal_process_group(process, signal.SIGTERM)
    try:
        process.wait(timeout=CHILD_STOP_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        logger.error("worker child ignored SIGTERM; killing its process group")
    else:
        # The Uvicorn leader can exit before a BabelDOC grandchild. Always
        # close the process group boundary before starting another worker.
        time.sleep(0.5)
    _signal_process_group(process, signal.SIGKILL)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        logger.critical("worker process group could not be reaped")


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
    return subprocess.Popen(command, start_new_session=True)


def main() -> int:
    if not WORKER_TOKEN:
        logger.critical("PDF2ZH_WORKER_TOKEN is required")
        return 64
    stopping = threading.Event()

    def request_stop(signum: int, _frame: object) -> None:
        logger.info("received signal %s; stopping worker", signum)
        stopping.set()

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)

    child = _spawn_child()
    logger.info("started worker child pid=%s", child.pid)
    startup_deadline = time.monotonic() + STARTUP_TIMEOUT_SECONDS
    ready = False
    failures = 0
    failure_reason = "worker child exited unexpectedly"

    while not stopping.is_set() and child.poll() is None:
        if _healthy():
            if not ready:
                logger.info("worker child is healthy")
            ready = True
            failures = 0
        elif not ready:
            if time.monotonic() >= startup_deadline:
                failure_reason = "worker did not become healthy before startup timeout"
                logger.error("%s; failing the container", failure_reason)
                _stop_child(child)
                break
        else:
            failures += 1
            if failures >= FAILURE_LIMIT:
                failure_reason = f"worker health failed {failures} consecutive checks"
                logger.error("%s; failing the container", failure_reason)
                _stop_child(child)
                break
        stopping.wait(CHECK_INTERVAL_SECONDS)

    if stopping.is_set():
        _stop_child(child)
        logger.info("worker supervisor stopped")
        return 0

    return_code = child.poll()
    _signal_process_group(child, signal.SIGKILL)
    logger.critical(
        "%s (child return code %s); exiting so Docker destroys the old cgroup",
        failure_reason,
        return_code,
    )
    return 70


if __name__ == "__main__":
    raise SystemExit(main())
