"""End-to-end regression for cancel-then-retry against a running Gradio UI."""

from __future__ import annotations

import argparse
import socket
import ssl
import struct
import time
from pathlib import Path
from urllib.parse import urlparse

from gradio_client import Client
from gradio_client import handle_file


def translation_args(client: Client, pdf: Path, pages: str) -> list[object]:
    endpoint = next(
        endpoint
        for endpoint in client.endpoints.values()
        if endpoint.api_name == "/translate_files"
    )
    args = [
        parameter["parameter_default"]
        if parameter["parameter_has_default"]
        else None
        for parameter in endpoint.parameters_info
    ]
    service = endpoint.parameters_info[3]
    assert service["type"]["enum"] == ["Google"], service["type"]["enum"]

    args[0] = "File"
    args[1] = [handle_file(str(pdf.resolve()))]
    args[2] = ""
    args[3] = "Google"
    args[6] = pages
    args[7] = ""
    args[24] = False  # Automatic term extraction stays disabled.
    return args


def wait_until_running(job, timeout: float = 60.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        code = str(job.status().code)
        if code.endswith("PROGRESS"):
            return
        if code.endswith(("FINISHED", "CANCELLED", "ERROR")):
            raise RuntimeError(f"Job stopped before cancellation test: {code}")
        time.sleep(0.5)
    raise TimeoutError(f"Job did not enter PROGRESS within {timeout}s")


def assert_retry_succeeds(
    client: Client, short_pdf: Path, timeout: float, label: str
) -> None:
    started = time.monotonic()
    retry = client.submit(
        *translation_args(client, short_pdf, "First"),
        api_name="/translate_files",
    )
    result = retry.result(timeout=timeout)
    elapsed = time.monotonic() - started
    if not result:
        raise RuntimeError(f"{label}: retry completed without a translation result")
    print(f"{label}=passed retry_seconds={elapsed:.2f}", flush=True)


def run_explicit_cancel_round(
    client: Client,
    long_pdf: Path,
    short_pdf: Path,
    cancel_after: float,
    retry_timeout: float,
    round_number: int,
) -> None:
    job = client.submit(
        *translation_args(client, long_pdf, "All"),
        api_name="/translate_files",
    )
    wait_until_running(job)
    time.sleep(cancel_after)
    print(
        f"round={round_number} status_before_cancel={job.status().code}", flush=True
    )

    # The UI button invokes both dependencies: our session-aware stop handler
    # performs bounded worker cleanup, while Gradio marks the queued event as
    # cancelled.  Use the same order here so cancellation cannot discard the
    # only server-side task handle before cleanup starts.
    client.predict(api_name="/stop_translate_file")
    native_cancelled = job.cancel()
    print(
        f"round={round_number} native_cancel={native_cancelled} "
        f"status_after_cancel={job.status().code}",
        flush=True,
    )
    assert_retry_succeeds(
        client, short_pdf, retry_timeout, f"explicit_cancel_round_{round_number}"
    )


def run_heartbeat_disconnect_round(
    base_url: str,
    long_pdf: Path,
    short_pdf: Path,
    retry_timeout: float,
) -> None:
    browser_session = Client(base_url, verbose=False)
    parsed_url = urlparse(base_url)
    host = parsed_url.hostname or "127.0.0.1"
    port = parsed_url.port or (443 if parsed_url.scheme == "https" else 80)
    heartbeat_path = (
        parsed_url.path.rstrip("/")
        + f"/gradio_api/heartbeat/{browser_session.session_hash}"
    )
    heartbeat = socket.create_connection((host, port), timeout=10)
    if parsed_url.scheme == "https":
        heartbeat = ssl.create_default_context().wrap_socket(
            heartbeat, server_hostname=host
        )
    heartbeat.settimeout(20)
    heartbeat.sendall(
        (
            f"GET {heartbeat_path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Accept: text/event-stream\r\n"
            "Connection: keep-alive\r\n\r\n"
        ).encode("ascii")
    )
    heartbeat_response = b""
    while b"ALIVE" not in heartbeat_response and len(heartbeat_response) < 65536:
        chunk = heartbeat.recv(4096)
        if not chunk:
            break
        heartbeat_response += chunk
    if b"ALIVE" not in heartbeat_response:
        heartbeat.close()
        raise RuntimeError("Heartbeat stream did not become alive")

    abandoned = browser_session.submit(
        *translation_args(browser_session, long_pdf, "All"),
        api_name="/translate_files",
    )
    wait_until_running(abandoned)
    # Force a TCP reset instead of relying on an HTTP client's connection pool.
    # This deterministically exercises Gradio's server-side heartbeat unload.
    linger = struct.pack("hh", 1, 0)
    try:
        heartbeat.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, linger)
    except OSError:
        heartbeat.setsockopt(
            socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0)
        )
    heartbeat.close()
    print("browser_heartbeat=closed", flush=True)

    # A fresh session must get the global slot after the heartbeat invokes the
    # Blocks.unload callback and the old worker has been reaped.
    fresh_session = Client(base_url, verbose=False)
    try:
        assert_retry_succeeds(
            fresh_session, short_pdf, retry_timeout, "heartbeat_disconnect_retry"
        )
    finally:
        # Stop the local gradio_client listener for the deliberately abandoned
        # job.  This happens only after the fresh-session assertion, so it
        # cannot help the server-side unload test pass.
        abandoned.cancel()
        browser_session.close()
        fresh_session.close()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("base_url")
    parser.add_argument("long_pdf", type=Path)
    parser.add_argument("short_pdf", type=Path)
    parser.add_argument("--cancel-after", type=float, default=8.0)
    parser.add_argument("--retry-timeout", type=float, default=180.0)
    parser.add_argument("--rounds", type=int, default=1)
    parser.add_argument("--test-heartbeat-disconnect", action="store_true")
    options = parser.parse_args()

    client = Client(options.base_url, verbose=False)
    try:
        for round_number in range(1, options.rounds + 1):
            run_explicit_cancel_round(
                client,
                options.long_pdf,
                options.short_pdf,
                options.cancel_after,
                options.retry_timeout,
                round_number,
            )
        if options.test_heartbeat_disconnect:
            run_heartbeat_disconnect_round(
                options.base_url,
                options.long_pdf,
                options.short_pdf,
                options.retry_timeout,
            )
    finally:
        client.close()


if __name__ == "__main__":
    main()
