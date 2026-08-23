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


def resolve_container(explicit: str | None, compose_project: str) -> str:
    if explicit:
        labels = json.loads(
            docker("inspect", "--format", "{{json .Config.Labels}}", explicit)
        )
        if not isinstance(labels, dict) or (
            labels.get("com.docker.compose.project") != compose_project
            or labels.get("com.docker.compose.service") != "translator"
        ):
            raise RuntimeError(
                "explicit container is not the translator service for Compose "
                f"project {compose_project!r}"
            )
        if docker("inspect", "--format", "{{.State.Running}}", explicit) != "true":
            raise RuntimeError("explicit translator container is not running")
        return explicit
    matches = docker(
        "ps",
        "--filter",
        f"label=com.docker.compose.project={compose_project}",
        "--filter",
        "label=com.docker.compose.service=translator",
        "--format",
        "{{.ID}}",
    ).splitlines()
    if len(matches) != 1:
        raise RuntimeError(
            "expected exactly one running translator container for Compose project "
            f"{compose_project!r}, found {len(matches)}"
        )
    return matches[0]


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


def assert_one_shot_process_tree(container: str) -> None:
    code = """\
import json, os
from pathlib import Path

processes = []
for entry in Path("/proc").iterdir():
    if not entry.name.isdigit() or int(entry.name) == os.getpid():
        continue
    try:
        command = (entry / "cmdline").read_bytes().replace(b"\\0", b" ").decode()
        status = (entry / "status").read_text()
    except (FileNotFoundError, PermissionError, ProcessLookupError, UnicodeError):
        continue
    parent = next(
        int(line.split(":", 1)[1])
        for line in status.splitlines()
        if line.startswith("PPid:")
    )
    status_fields = {
        line.split(":", 1)[0]: line.split(":", 1)[1].strip()
        for line in status.splitlines()
        if ":" in line
    }
    processes.append({
        "pid": int(entry.name),
        "ppid": parent,
        "command": command,
        "uids": [int(value) for value in status_fields["Uid"].split()],
        "gids": [int(value) for value in status_fields["Gid"].split()],
        "groups": [int(value) for value in status_fields.get("Groups", "").split()],
        "cap_eff": int(status_fields["CapEff"], 16),
        "cap_prm": int(status_fields["CapPrm"], 16),
        "cap_bnd": int(status_fields["CapBnd"], 16),
        "cap_inh": int(status_fields["CapInh"], 16),
        "cap_amb": int(status_fields.get("CapAmb", "0"), 16),
        "no_new_privs": int(status_fields["NoNewPrivs"]),
    })

init_process = next(process for process in processes if process["pid"] == 1)
supervisors = [
    process for process in processes
    if process["pid"] != 1
    and (
        "pdf2zh_next.worker_supervisor" in process["command"]
        or "/pdf2zh_next/worker_supervisor.py" in process["command"]
    )
]
uvicorn = [
    process for process in processes
    if "uvicorn" in process["command"]
    and "pdf2zh_next.worker_api:app" in process["command"]
]
valid_init = "tini" in init_process["command"] or "docker-init" in init_process["command"]
supervisor_capability_mask = (1 << 5) | (1 << 6) | (1 << 7)
valid = (
    valid_init
    and len(supervisors) == 1
    and supervisors[0]["ppid"] == 1
    and set(supervisors[0]["uids"]) == {0}
    and set(supervisors[0]["gids"]) == {0}
    and set(supervisors[0]["groups"]).issubset({0})
    and supervisors[0]["cap_eff"] == supervisor_capability_mask
    and supervisors[0]["cap_prm"] == supervisor_capability_mask
    and supervisors[0]["cap_bnd"] == supervisor_capability_mask
    and supervisors[0]["cap_inh"] == 0
    and supervisors[0]["cap_amb"] == 0
    and supervisors[0]["no_new_privs"] == 1
    and len(uvicorn) == 1
    and uvicorn[0]["ppid"] == supervisors[0]["pid"]
    and set(uvicorn[0]["uids"]) == {10001}
    and set(uvicorn[0]["gids"]) == {10001}
    and set(uvicorn[0]["groups"]).issubset({10001})
    and uvicorn[0]["cap_eff"] == 0
    and uvicorn[0]["cap_prm"] == 0
    and uvicorn[0]["cap_inh"] == 0
    and uvicorn[0]["cap_amb"] == 0
    and uvicorn[0]["no_new_privs"] == 1
)
print(json.dumps({
    "valid": valid,
    "init": init_process,
    "supervisors": supervisors,
    "uvicorn": uvicorn,
}))
"""
    result = json.loads(docker("exec", container, "python", "-c", code))
    if not result["valid"]:
        raise RuntimeError(
            f"worker is not a one-shot supervised process tree: {result}"
        )


def wait_for_new_worker_generation(
    container: str,
    *,
    previous_restart_count: int,
    previous_health: dict,
    timeout: float,
) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if restart_count(container) > previous_restart_count:
                candidate = worker_health(container)
                if candidate.get("epoch") != previous_health.get(
                    "epoch"
                ) and candidate.get("container_boot_id") != previous_health.get(
                    "container_boot_id"
                ):
                    return candidate
        except (subprocess.CalledProcessError, ValueError, json.JSONDecodeError):
            pass
        time.sleep(2)
    raise TimeoutError(
        "worker container did not restart with a new epoch and container boot id"
    )


def wait_for_ready_worker(
    base_url: str,
    explicit_container: str | None,
    compose_project: str,
    timeout: float,
) -> tuple[str, int, dict]:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            coordinator_health = request_json(f"{base_url}/api/health")
            if (
                coordinator_health.get("provider") != "google"
                or coordinator_health.get("activeJobs") != 0
            ):
                raise RuntimeError(
                    f"coordinator is not idle and ready: {coordinator_health}"
                )

            container = resolve_container(explicit_container, compose_project)
            initial_restart = restart_count(container)
            initial_health = worker_health(container)
            if (
                initial_health.get("status") != "ok"
                or initial_health.get("provider") != "google"
                or initial_health.get("max_active_jobs") != 1
                or initial_health.get("active_jobs") != 0
                or not initial_health.get("epoch")
                or not initial_health.get("container_boot_id")
            ):
                raise RuntimeError(f"worker is not idle and ready: {initial_health}")
            assert_one_shot_process_tree(container)

            confirmed_restart = restart_count(container)
            confirmed_health = worker_health(container)
            if (
                confirmed_restart != initial_restart
                or confirmed_health.get("epoch") != initial_health.get("epoch")
                or confirmed_health.get("container_boot_id")
                != initial_health.get("container_boot_id")
            ):
                raise RuntimeError("worker generation changed during readiness probe")
            return container, confirmed_restart, confirmed_health
        except (
            OSError,
            RuntimeError,
            ValueError,
            json.JSONDecodeError,
            subprocess.CalledProcessError,
        ) as error:
            last_error = error
        time.sleep(2)
    raise TimeoutError(
        "worker did not become stably ready before fail-stop test"
    ) from last_error


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("base_url")
    parser.add_argument("long_pdf", type=Path)
    parser.add_argument("--container")
    parser.add_argument("--compose-project", default="pdf2zh-next")
    parser.add_argument("--restart-timeout", type=float, default=240)
    parser.add_argument("--terminal-timeout", type=float, default=180)
    options = parser.parse_args()
    base_url = options.base_url.rstrip("/")
    container, before_restart, before_health = wait_for_ready_worker(
        base_url,
        options.container,
        options.compose_project,
        options.restart_timeout,
    )

    job = submit(
        base_url,
        options.long_pdf,
        admission_timeout=options.restart_timeout,
    )
    wait_for_status(base_url, job["id"], {"running"}, 120)
    pid = uvicorn_pid(container)
    docker("exec", container, "sh", "-c", f"kill -STOP {pid}")
    print(f"stopped_worker_pid={pid}", flush=True)

    deadline = time.monotonic() + options.restart_timeout
    while restart_count(container) == before_restart:
        try:
            request_json(
                f"{base_url}/api/jobs/{job['id']}/heartbeat",
                method="POST",
                data=b"",
            )
        except Exception:
            pass
        if time.monotonic() >= deadline:
            raise TimeoutError("stalled worker container did not restart")
        time.sleep(1)
    after_health = wait_for_new_worker_generation(
        container,
        previous_restart_count=before_restart,
        previous_health=before_health,
        timeout=options.restart_timeout,
    )
    assert_one_shot_process_tree(container)
    print(
        f"worker_restart_count={restart_count(container)} "
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

    # The Rust terminal snapshot can precede replacement-worker drain/scrub
    # and admission release. submit() waits for readiness and then retries the
    # same UUID and multipart bytes across any remaining 409/429/503 race.
    probe = submit(
        base_url,
        options.long_pdf,
        admission_timeout=options.restart_timeout,
    )
    wait_for_status(base_url, probe["id"], {"running"}, 120)
    probe_restart = restart_count(container)
    probe_health = worker_health(container)
    cancel_and_wait(base_url, probe["id"], 120)
    final_health = wait_for_new_worker_generation(
        container,
        previous_restart_count=probe_restart,
        previous_health=probe_health,
        timeout=options.restart_timeout,
    )
    if final_health.get("active_jobs") != 0:
        raise RuntimeError(f"worker retained an active job: {final_health}")
    assert_one_shot_process_tree(container)
    print("post_failstop_probe_and_terminal_recycle=passed", flush=True)


if __name__ == "__main__":
    main()
