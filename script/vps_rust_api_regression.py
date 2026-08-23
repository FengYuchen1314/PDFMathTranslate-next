"""VPS end-to-end regression for the Rust/FastAPI web architecture."""

from __future__ import annotations

import argparse
import json
import mimetypes
import time
import uuid
from pathlib import Path
from urllib.error import HTTPError
from urllib.error import URLError
from urllib.parse import urlsplit
from urllib.request import Request
from urllib.request import urlopen

TRANSIENT_ADMISSION_STATUSES = {409, 429, 503}
DEFAULT_ADMISSION_TIMEOUT = 240.0


def request_json(
    url: str, method: str = "GET", data: bytes | None = None, headers=None
):
    request = Request(  # noqa: S310 - the caller validates the HTTP(S) base URL
        url, data=data, method=method, headers=headers or {}
    )
    try:
        with urlopen(request, timeout=60) as response:  # noqa: S310
            body = response.read()
            return json.loads(body) if body else None
    except HTTPError as error:
        body = error.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"{method} {url} failed: {error.code} {body}") from error


def request_status(
    url: str, method: str = "GET", data: bytes | None = None, headers=None
) -> tuple[int, bytes]:
    request = Request(  # noqa: S310 - main() validates the HTTP(S) base URL
        url, data=data, method=method, headers=headers or {}
    )
    try:
        with urlopen(request, timeout=60) as response:  # noqa: S310
            return response.status, response.read()
    except HTTPError as error:
        return error.code, error.read()


def expect_status(
    expected: int,
    url: str,
    method: str = "GET",
    data: bytes | None = None,
    headers=None,
) -> bytes:
    actual, body = request_status(url, method=method, data=data, headers=headers)
    if actual != expected:
        raise RuntimeError(
            f"{method} {url} returned {actual}, expected {expected}: "
            f"{body.decode('utf-8', errors='replace')}"
        )
    return body


def multipart_payload(
    pdf: Path,
    pages: str,
    provider_id: str = "google",
    model_id: str | None = None,
    thinking_mode: str | None = None,
    reasoning_effort: str | None = None,
) -> tuple[bytes, str]:
    boundary = f"----pdf2zh-{uuid.uuid4().hex}"
    chunks: list[bytes] = []
    fields = {
        "source_lang": "en",
        "target_lang": "zh",
        "pages": pages,
        "output_mono": "true",
        "output_dual": "true",
        "provider_id": provider_id,
    }
    if model_id:
        fields["model_id"] = model_id
    if thinking_mode:
        fields["thinking_mode"] = thinking_mode
    if reasoning_effort:
        fields["reasoning_effort"] = reasoning_effort
    for name, value in fields.items():
        chunks.extend(
            [
                f"--{boundary}\r\n".encode(),
                f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode(),
                value.encode(),
                b"\r\n",
            ]
        )
    media_type = mimetypes.guess_type(pdf.name)[0] or "application/pdf"
    chunks.extend(
        [
            f"--{boundary}\r\n".encode(),
            (
                f'Content-Disposition: form-data; name="file"; filename="{pdf.name}"\r\n'
                f"Content-Type: {media_type}\r\n\r\n"
            ).encode(),
            pdf.read_bytes(),
            b"\r\n",
            f"--{boundary}--\r\n".encode(),
        ]
    )
    return b"".join(chunks), boundary


def minimal_multipart_payload() -> tuple[bytes, str]:
    """Build a tiny body for requests rejected before multipart is consumed."""
    boundary = f"----pdf2zh-rejected-{uuid.uuid4().hex}"
    return f"--{boundary}--\r\n".encode(), boundary


def wait_for_admission_ready(base_url: str, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last_status = 0
    last_body = b""
    while True:
        last_status, last_body = request_status(f"{base_url}/api/health")
        if last_status == 200:
            try:
                health = json.loads(last_body)
            except (TypeError, ValueError) as error:
                raise RuntimeError("health endpoint returned invalid JSON") from error
            if isinstance(health, dict) and health.get("activeJobs") == 0:
                return
        elif last_status not in {429, 503}:
            raise RuntimeError(
                f"GET {base_url}/api/health returned {last_status}: "
                f"{last_body.decode('utf-8', errors='replace')}"
            )
        if time.monotonic() >= deadline:
            raise TimeoutError(
                "coordinator did not expose a ready worker with zero active jobs: "
                f"HTTP {last_status} {last_body.decode('utf-8', errors='replace')}"
            )
        time.sleep(1)


def submit(
    base_url: str,
    pdf: Path,
    pages: str = "",
    job_id: str | None = None,
    provider_id: str = "google",
    model_id: str | None = None,
    thinking_mode: str | None = None,
    reasoning_effort: str | None = None,
    admission_timeout: float = DEFAULT_ADMISSION_TIMEOUT,
) -> dict:
    payload, boundary = multipart_payload(
        pdf,
        pages,
        provider_id=provider_id,
        model_id=model_id,
        thinking_mode=thinking_mode,
        reasoning_effort=reasoning_effort,
    )
    job_id = job_id or str(uuid.uuid4())
    wait_for_admission_ready(base_url, admission_timeout)
    headers = {
        "Content-Type": f"multipart/form-data; boundary={boundary}",
        "x-job-id": job_id,
    }
    deadline = time.monotonic() + admission_timeout
    last_status = 0
    last_body = b""
    while True:
        try:
            last_status, last_body = request_status(
                f"{base_url}/api/jobs",
                method="POST",
                data=payload,
                headers=headers,
            )
        except (ConnectionError, TimeoutError, URLError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(1)
            continue
        if last_status in {200, 201, 202}:
            try:
                result = json.loads(last_body)
            except (TypeError, ValueError) as error:
                raise RuntimeError("job submission returned invalid JSON") from error
            if not isinstance(result, dict) or result.get("id") != job_id:
                raise RuntimeError(
                    f"job submission returned a mismatched response: {result!r}"
                )
            return result
        if (
            last_status not in TRANSIENT_ADMISSION_STATUSES
            or time.monotonic() >= deadline
        ):
            break
        # A terminal snapshot is published before the old worker generation is
        # destroyed, its replacement is drained/scrubbed/resumed, and the
        # global admission permit is released. Retrying this exact UUID and
        # byte-identical multipart body is safe even if an earlier response was
        # lost after the server accepted it.
        time.sleep(1)
    raise RuntimeError(
        f"POST {base_url}/api/jobs did not become admissible: {last_status} "
        f"{last_body.decode('utf-8', errors='replace')}"
    )


def probe_idempotent_create(base_url: str, job_id: str) -> dict:
    payload, boundary = minimal_multipart_payload()
    return request_json(
        f"{base_url}/api/jobs",
        method="POST",
        data=payload,
        headers={
            "Content-Type": f"multipart/form-data; boundary={boundary}",
            "x-job-id": job_id,
        },
    )


def get_job(base_url: str, job_id: str) -> dict:
    return request_json(f"{base_url}/api/jobs/{job_id}")


def wait_for_status(
    base_url: str,
    job_id: str,
    wanted: set[str],
    timeout: float,
    keep_alive: bool = True,
) -> dict:
    deadline = time.monotonic() + timeout
    last_revision = None
    while time.monotonic() < deadline:
        job = get_job(base_url, job_id)
        if job.get("revision") != last_revision:
            print(
                f"job={job_id[:8]} status={job['status']} "
                f"progress={job.get('progress', 0):.1f} stage={job.get('stage', '')}",
                flush=True,
            )
            last_revision = job.get("revision")
        if job["status"] in wanted:
            return job
        if job["status"] in {"failed", "cancelled", "succeeded"}:
            raise RuntimeError(
                f"job {job_id} reached unexpected terminal state {job['status']}: "
                f"{job.get('error') or job.get('message')}"
            )
        if keep_alive:
            request_json(
                f"{base_url}/api/jobs/{job_id}/heartbeat",
                method="POST",
                data=b"",
            )
        time.sleep(0.5)
    raise TimeoutError(f"job {job_id} did not reach {sorted(wanted)} in {timeout}s")


def cancel_and_wait(base_url: str, job_id: str, timeout: float) -> dict:
    request_json(f"{base_url}/api/jobs/{job_id}/cancel", method="POST", data=b"")
    return wait_for_status(base_url, job_id, {"cancelled"}, timeout)


def assert_download(base_url: str, job_id: str, kind: str) -> None:
    with urlopen(  # noqa: S310 - base_url is validated as HTTP(S) by main()
        f"{base_url}/api/jobs/{job_id}/files/{kind}", timeout=60
    ) as response:
        prefix = response.read(5)
        if prefix != b"%PDF-":
            raise RuntimeError(f"{kind} artifact is not a PDF: {prefix!r}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("base_url")
    parser.add_argument("long_pdf", type=Path)
    parser.add_argument("short_pdf", type=Path)
    parser.add_argument(
        "--lease-pdf",
        type=Path,
        help=(
            "optional PDF guaranteed to run longer than the deployment lease; "
            "used only for the abandoned-job reclamation check"
        ),
    )
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--start-timeout", type=float, default=120)
    parser.add_argument("--cancel-timeout", type=float, default=90)
    parser.add_argument("--finish-timeout", type=float, default=600)
    parser.add_argument(
        "--lease-timeout",
        type=float,
        default=360,
        help=(
            "seconds allowed for an abandoned job to exceed the configured "
            "lease and finish cancellation (default covers the 180s VPS lease)"
        ),
    )
    parser.add_argument("--test-lease", action="store_true")
    options = parser.parse_args()
    base_url = options.base_url.rstrip("/")
    parsed_base_url = urlsplit(base_url)
    if parsed_base_url.scheme not in {"http", "https"} or not parsed_base_url.netloc:
        raise ValueError("base_url must be an absolute HTTP(S) URL")

    if parsed_base_url.scheme == "https":
        with urlopen(f"{base_url}/", timeout=60) as response:  # noqa: S310
            headers = response.headers
            assert "max-age=" in headers.get("Strict-Transport-Security", "")
            assert "default-src 'self'" in headers.get("Content-Security-Policy", "")
            assert headers.get("X-Content-Type-Options") == "nosniff"
            assert headers.get("X-Frame-Options") == "DENY"
        print("gateway_security_headers=passed", flush=True)

    health = request_json(f"{base_url}/api/health")
    config = request_json(f"{base_url}/api/config")
    assert health["provider"] == "google", health
    assert config["provider"] == "google", config
    assert "services" not in config, config
    providers = {provider["id"]: provider for provider in config["providers"]}
    assert set(providers) == {"google"}, providers
    assert providers["google"]["available"] is True, providers["google"]
    assert "csrfToken" not in config, config
    assert "apiKey" not in json.dumps(config), config
    assert "baseUrl" not in json.dumps(config), config
    for provider_id, provider in providers.items():
        assert provider["id"] == provider_id, provider
        assert provider["kind"] in {"google", "deepseek", "openai_compatible"}, provider
        if provider["kind"] == "openai_compatible":
            assert provider["models"], provider
            assert provider.get("defaultModel") in {
                model["id"] for model in provider["models"]
            }, provider
    print("anonymous_google_only_contract=passed", flush=True)

    # A release/cancel beacon can beat the matching upload POST when a page is
    # closed immediately. Both UUIDs must be durably retired before any work
    # starts, and delayed POSTs must receive 410 instead of creating an orphan.
    for endpoint in ("release", "cancel"):
        retired_id = str(uuid.uuid4())
        expect_status(
            202,
            f"{base_url}/api/jobs/{retired_id}/{endpoint}",
            method="POST",
            data=b"",
        )
        payload, boundary = minimal_multipart_payload()
        expect_status(
            410,
            f"{base_url}/api/jobs",
            method="POST",
            data=payload,
            headers={
                "Content-Type": f"multipart/form-data; boundary={boundary}",
                "x-job-id": retired_id,
            },
        )
    print("prestart_release_cancel_tombstones=passed", flush=True)

    for round_number in range(1, options.rounds + 1):
        first = submit(base_url, options.long_pdf)
        duplicate = probe_idempotent_create(base_url, first["id"])
        if duplicate["id"] != first["id"]:
            raise RuntimeError(f"idempotent create returned a new job: {duplicate}")
        wait_for_status(
            base_url,
            first["id"],
            {"running"},
            options.start_timeout,
        )
        cancel_and_wait(base_url, first["id"], options.cancel_timeout)

        # The immediate probe must enter the global single-concurrency slot.
        probe = submit(base_url, options.long_pdf)
        wait_for_status(
            base_url,
            probe["id"],
            {"running"},
            options.start_timeout,
        )
        cancel_and_wait(base_url, probe["id"], options.cancel_timeout)
        print(f"idempotent_create_round_{round_number}=passed", flush=True)
        print(f"cancel_retry_round_{round_number}=passed", flush=True)

    if options.test_lease:
        refreshable = submit(base_url, options.long_pdf)
        wait_for_status(
            base_url,
            refreshable["id"],
            {"running"},
            options.start_timeout,
        )
        request_json(
            f"{base_url}/api/jobs/{refreshable['id']}/release",
            method="POST",
            data=b"",
        )
        time.sleep(2)
        request_json(
            f"{base_url}/api/jobs/{refreshable['id']}/heartbeat",
            method="POST",
            data=b"",
        )
        refreshed = get_job(base_url, refreshable["id"])
        if refreshed["status"] not in {"queued", "running"}:
            raise RuntimeError(
                f"release grace did not allow refresh recovery: {refreshed}"
            )
        cancel_and_wait(base_url, refreshable["id"], options.cancel_timeout)
        print("release_refresh_grace=passed", flush=True)

        released = submit(base_url, options.long_pdf)
        wait_for_status(
            base_url,
            released["id"],
            {"running"},
            options.start_timeout,
        )
        request_json(
            f"{base_url}/api/jobs/{released['id']}/release",
            method="POST",
            data=b"",
        )
        wait_for_status(
            base_url,
            released["id"],
            {"cancelled"},
            options.cancel_timeout,
            keep_alive=False,
        )
        print("released_page_reclaimed=passed", flush=True)

        abandoned = submit(base_url, options.lease_pdf or options.long_pdf)
        wait_for_status(
            base_url,
            abandoned["id"],
            {"running"},
            options.start_timeout,
        )
        wait_for_status(
            base_url,
            abandoned["id"],
            {"cancelled"},
            options.lease_timeout,
            keep_alive=False,
        )
        lease_probe = submit(base_url, options.long_pdf)
        wait_for_status(
            base_url,
            lease_probe["id"],
            {"running"},
            options.start_timeout,
        )
        cancel_and_wait(base_url, lease_probe["id"], options.cancel_timeout)
        print("abandoned_lease_retry=passed", flush=True)

    completed = submit(base_url, options.short_pdf, pages="1")
    result = wait_for_status(
        base_url,
        completed["id"],
        {"succeeded"},
        options.finish_timeout,
    )
    if result.get("files", {}).get("mono"):
        assert_download(base_url, completed["id"], "mono")
    if result.get("files", {}).get("dual"):
        assert_download(base_url, completed["id"], "dual")
    if not result.get("files", {}).get("mono") and not result.get("files", {}).get(
        "dual"
    ):
        raise RuntimeError("completed job exposed no PDF artifacts")
    print("successful_translation_and_download=passed", flush=True)

    # Terminal release is recoverable during the grace period. Explicit DELETE
    # is the irreversible cleanup operation and leaves an ID tombstone.
    request_json(
        f"{base_url}/api/jobs/{completed['id']}/release",
        method="POST",
        data=b"",
    )
    time.sleep(2)
    request_json(
        f"{base_url}/api/jobs/{completed['id']}/heartbeat",
        method="POST",
        data=b"",
    )
    if result.get("files", {}).get("mono"):
        assert_download(base_url, completed["id"], "mono")
    else:
        assert_download(base_url, completed["id"], "dual")
    print("terminal_release_refresh_download=passed", flush=True)

    expect_status(
        204,
        f"{base_url}/api/jobs/{completed['id']}",
        method="DELETE",
    )
    expect_status(410, f"{base_url}/api/jobs/{completed['id']}")
    payload, boundary = minimal_multipart_payload()
    expect_status(
        410,
        f"{base_url}/api/jobs",
        method="POST",
        data=payload,
        headers={
            "Content-Type": f"multipart/form-data; boundary={boundary}",
            "x-job-id": completed["id"],
        },
    )
    print("explicit_delete_and_id_tombstone=passed", flush=True)


if __name__ == "__main__":
    main()
