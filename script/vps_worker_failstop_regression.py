"""Exercise worker fail-stop recovery against the VPS Docker deployment."""

from __future__ import annotations

import argparse
import json
import subprocess  # noqa: S404 - fixed Docker CLI invocation on the VPS
import time
from pathlib import Path

from vps_rust_api_regression import cancel_and_wait
from vps_rust_api_regression import request_json
from vps_rust_api_regression import submit
from vps_rust_api_regression import wait_for_status


def docker(*arguments: str) -> str:
    completed = subprocess.run(  # noqa: S603 - arguments are a fixed list
        ["/usr/bin/docker", *arguments],
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout.strip()


def restart_count(container: str) -> int:
    return int(docker("inspect", "--format", "{{.RestartCount}}", container))


def worker_health(container: str) -> dict:
    code = """\
import json, os, urllib.request
request = urllib.request.Request(
    "http://127.0.0.1:8000/internal/health",
    headers={"x-worker-token": os.environ["PDF2ZH_WORKER_TOKEN"]},
)
with urllib.request.urlopen(request, timeout=3) as response:
    print(json.dumps(json.load(response)))
"""
    return json.loads(docker("exec", container, "python", "-c", code))


def uvicorn_pid(container: str) -> int:
    code = """\
import os
from pathlib import Path
matches = []
for child in Path("/proc").iterdir():
    if not child.name.isdigit():
        continue
    if int(child.name) == os.getpid():
        continue
    try:
        command = (child / "cmdline").read_bytes().replace(b"\\0", b" ").decode()
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        continue
    if "uvicorn" in command and "pdf2zh_next.worker_api:app" in command:
        matches.append(int(child.name))
if len(matches) != 1:
    raise SystemExit(f"expected one Uvicorn process, found {matches}")
print(matches[0])
"""
    return int(docker("exec", container, "python", "-c", code))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("base_url")
    parser.add_argument("long_pdf", type=Path)
    parser.add_argument("--container", default="pdf2zh-next-translator")
    parser.add_argument("--restart-timeout", type=float, default=150)
    parser.add_argument("--terminal-timeout", type=float, default=180)
    options = parser.parse_args()
    base_url = options.base_url.rstrip("/")

    before_restart = restart_count(options.container)
    before_health = worker_health(options.container)
    before_epoch = before_health["epoch"]

    job = submit(base_url, options.long_pdf)
    wait_for_status(base_url, job["id"], {"running"}, 120)
    pid = uvicorn_pid(options.container)
    docker("exec", options.container, "sh", "-c", f"kill -STOP {pid}")
    print(f"stopped_worker_pid={pid}", flush=True)

    deadline = time.monotonic() + options.restart_timeout
    after_health = None
    while time.monotonic() < deadline:
        try:
            request_json(
                f"{base_url}/api/jobs/{job['id']}/heartbeat",
                method="POST",
                data=b"",
            )
        except Exception:
            pass
        try:
            if restart_count(options.container) > before_restart:
                candidate = worker_health(options.container)
                if candidate.get("epoch") != before_epoch:
                    after_health = candidate
                    break
        except (subprocess.CalledProcessError, ValueError, json.JSONDecodeError):
            pass
        time.sleep(2)
    if after_health is None:
        raise TimeoutError("worker container did not restart with a new epoch")
    print(
        f"worker_restart_count={restart_count(options.container)} "
        f"new_epoch={after_health['epoch']}",
        flush=True,
    )

    terminal = wait_for_status(
        base_url,
        job["id"],
        {"cancelled", "failed"},
        options.terminal_timeout,
    )
    print(f"stalled_job_terminal={terminal['status']}", flush=True)

    probe = submit(base_url, options.long_pdf)
    wait_for_status(base_url, probe["id"], {"running"}, 120)
    cancel_and_wait(base_url, probe["id"], 120)
    final_health = worker_health(options.container)
    if final_health.get("active_jobs") != 0:
        raise RuntimeError(f"worker retained an active job: {final_health}")
    print("post_failstop_probe=passed", flush=True)


if __name__ == "__main__":
    main()
