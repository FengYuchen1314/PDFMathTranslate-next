"""VPS end-to-end regression for the Rust/FastAPI web architecture."""

from __future__ import annotations

import argparse
import json
import mimetypes
import time
import uuid
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import Request
from urllib.request import urlopen


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
    provider: str = "google",
    deepseek_model: str | None = None,
    deepseek_thinking_mode: str | None = None,
) -> tuple[bytes, str]:
    boundary = f"----pdf2zh-{uuid.uuid4().hex}"
    chunks: list[bytes] = []
    fields = {
        "source_lang": "en",
        "target_lang": "zh",
        "pages": pages,
        "output_mono": "true",
        "output_dual": "true",
        "provider": provider,
    }
    if provider == "deepseek":
        fields["deepseek_model"] = deepseek_model or "deepseek-v4-flash"
        if deepseek_thinking_mode:
            fields["deepseek_thinking_mode"] = deepseek_thinking_mode
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


def submit(
    base_url: str,
    pdf: Path,
    pages: str = "",
    job_id: str | None = None,
    provider: str = "google",
    deepseek_model: str | None = None,
    deepseek_thinking_mode: str | None = None,
) -> dict:
    payload, boundary = multipart_payload(
        pdf,
        pages,
        provider=provider,
        deepseek_model=deepseek_model,
        deepseek_thinking_mode=deepseek_thinking_mode,
    )
    job_id = job_id or str(uuid.uuid4())
    return request_json(
        f"{base_url}/api/jobs",
        method="POST",
        data=payload,
        headers={
            "Content-Type": f"multipart/form-data; boundary={boundary}",
            "x-job-id": job_id,
        },
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
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--start-timeout", type=float, default=120)
    parser.add_argument("--cancel-timeout", type=float, default=90)
    parser.add_argument("--finish-timeout", type=float, default=600)
    parser.add_argument("--test-lease", action="store_true")
    parser.add_argument("--expect-deepseek", action="store_true")
    parser.add_argument(
        "--deepseek-model",
        choices=("deepseek-v4-flash", "deepseek-v4-pro"),
        default="deepseek-v4-flash",
    )
    options = parser.parse_args()
    base_url = options.base_url.rstrip("/")
    parsed_base_url = urlsplit(base_url)
    if parsed_base_url.scheme not in {"http", "https"} or not parsed_base_url.netloc:
        raise ValueError("base_url must be an absolute HTTP(S) URL")

    health = request_json(f"{base_url}/api/health")
    config = request_json(f"{base_url}/api/config")
    assert health["provider"] == "google", health
    assert config["provider"] == "google", config
    assert "services" not in config, config
    providers = {provider["id"]: provider for provider in config["providers"]}
    assert set(providers) == {"google", "deepseek"}, providers
    assert providers["google"]["available"] is True, providers["google"]
    deepseek = providers["deepseek"]
    assert isinstance(deepseek["available"], bool), deepseek
    assert deepseek["defaultModel"] == "deepseek-v4-flash", deepseek
    assert [model["id"] for model in deepseek["models"]] == [
        "deepseek-v4-flash",
        "deepseek-v4-pro",
    ], deepseek
    assert deepseek["thinkingModes"] == ["enabled", "disabled"], deepseek
    assert deepseek["reasoningEfforts"] == ["low", "high", "max"], deepseek
    assert deepseek["defaultReasoningEffort"] == "high", deepseek
    assert "apiKey" not in json.dumps(config), config
    if options.expect_deepseek:
        assert deepseek["available"] is True, deepseek
    print(
        f"provider_contract=passed deepseek_available={deepseek['available']}",
        flush=True,
    )

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

        abandoned = submit(base_url, options.long_pdf)
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
            options.cancel_timeout,
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

    if options.expect_deepseek:
        deepseek_job = submit(
            base_url,
            options.short_pdf,
            pages="1",
            provider="deepseek",
            deepseek_model=options.deepseek_model,
            deepseek_thinking_mode="disabled",
        )
        deepseek_result = wait_for_status(
            base_url,
            deepseek_job["id"],
            {"succeeded"},
            options.finish_timeout,
        )
        if deepseek_result.get("options", {}).get("provider") != "deepseek":
            raise RuntimeError(
                f"DeepSeek job lost its provider snapshot: {deepseek_result}"
            )
        if deepseek_result.get("files", {}).get("mono"):
            assert_download(base_url, deepseek_job["id"], "mono")
        elif deepseek_result.get("files", {}).get("dual"):
            assert_download(base_url, deepseek_job["id"], "dual")
        else:
            raise RuntimeError("DeepSeek job exposed no PDF artifacts")
        expect_status(
            204,
            f"{base_url}/api/jobs/{deepseek_job['id']}",
            method="DELETE",
        )
        print(
            f"deepseek_live_translation={options.deepseek_model}=passed",
            flush=True,
        )

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
