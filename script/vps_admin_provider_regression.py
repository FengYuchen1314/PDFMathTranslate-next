#!/usr/bin/env python3
"""Black-box regression checks for the private admin/provider control plane.

Run this only against a disposable deployment whose state volume can be removed
afterwards. The script never prints the setup token, passwords, or API keys.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import http.cookiejar
import ipaddress
import json
import os
import pathlib
import stat
import subprocess  # noqa: S404 - fixed Docker CLI invocation on the VPS
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass
from typing import Any

TRANSIENT_ADMISSION_STATUSES = {409, 429, 503}
ADMISSION_TIMEOUT_SECONDS = 240


class RegressionFailure(RuntimeError):  # noqa: N818 - regression script contract
    pass


@dataclass(slots=True)
class HttpFailure(Exception):  # noqa: N818 - carries an HTTP response for assertions
    status: int
    headers: ResponseHeaders


@dataclass(slots=True)
class ResponseHeaders:
    values: dict[str, list[str]]

    @classmethod
    def from_message(cls, message: Any) -> ResponseHeaders:
        values: dict[str, list[str]] = {}
        for name, value in message.items():
            values.setdefault(name.lower(), []).append(value)
        return cls(values)

    def get(self, name: str, default: str = "") -> str:
        items = self.values.get(name.lower())
        return ", ".join(items) if items else default

    def get_all(self, name: str) -> list[str]:
        return list(self.values.get(name.lower(), []))


class AdminClient:
    def __init__(self, base_url: str, forbidden_response_values: list[str]) -> None:
        self.base_url = base_url.rstrip("/")
        parsed = urllib.parse.urlsplit(self.base_url)
        self.origin = f"{parsed.scheme}://{parsed.netloc}"
        self.cookies = http.cookiejar.CookieJar()
        self.forbidden_response_values = [
            value.encode() for value in forbidden_response_values if value
        ]
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(self.cookies)
        )

    def request(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None = None,
        *,
        csrf: str | None = None,
        origin: str | None = None,
    ) -> tuple[int, ResponseHeaders, Any]:
        body = None
        headers = {"Accept": "application/json"}
        if payload is not None:
            body = json.dumps(payload, separators=(",", ":")).encode()
            headers["Content-Type"] = "application/json"
        if method not in {"GET", "HEAD"}:
            headers["Origin"] = self.origin if origin is None else origin
        if csrf:
            headers["X-CSRF-Token"] = csrf
        request = urllib.request.Request(  # noqa: S310 - main validates HTTP(S) base
            f"{self.base_url}{path}", data=body, headers=headers, method=method
        )
        return self._open(request)

    def submit_probe_job(
        self,
        provider_id: str,
        *,
        model_id: str | None = None,
        csrf: str | None = None,
        origin: str | None = None,
    ) -> tuple[int, ResponseHeaders, Any]:
        job_id = str(uuid.uuid4())
        boundary = f"pdf2zh-regression-{job_id}"
        fields = {
            "source_lang": "auto",
            "target_lang": "zh",
            "pages": "",
            "output_mono": "true",
            "output_dual": "true",
            "provider_id": provider_id,
        }
        if model_id:
            fields["model_id"] = model_id
        chunks: list[bytes] = []
        for name, value in fields.items():
            chunks.extend(
                [
                    f"--{boundary}\r\n".encode(),
                    f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode(),
                    value.encode(),
                    b"\r\n",
                ]
            )
        chunks.extend(
            [
                f"--{boundary}\r\n".encode(),
                b'Content-Disposition: form-data; name="file"; filename="probe.pdf"\r\n',
                b"Content-Type: application/pdf\r\n\r\n",
                b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n",
                b"\r\n",
                f"--{boundary}--\r\n".encode(),
            ]
        )
        headers = {
            "Accept": "application/json",
            "Content-Type": f"multipart/form-data; boundary={boundary}",
            "Origin": self.origin if origin is None else origin,
            "X-Job-Id": job_id,
        }
        if csrf:
            headers["X-CSRF-Token"] = csrf
        body = b"".join(chunks)
        deadline = time.monotonic() + ADMISSION_TIMEOUT_SECONDS
        while True:
            request = urllib.request.Request(  # noqa: S310 - validated HTTP(S) base
                f"{self.base_url}/api/jobs",
                data=body,
                headers=headers,
                method="POST",
            )
            try:
                return self._open(request)
            except HttpFailure as error:
                if (
                    error.status not in TRANSIENT_ADMISSION_STATUSES
                    or time.monotonic() >= deadline
                ):
                    raise
                # A terminal job becomes visible before the old worker cgroup
                # has been replaced and the coordinator's admission permit is
                # released. Keep both the UUID and multipart bytes identical so
                # a response lost after acceptance cannot create duplicate work.
                time.sleep(1)
            except (ConnectionError, TimeoutError, urllib.error.URLError):
                if time.monotonic() >= deadline:
                    raise
                time.sleep(1)

    def _open(
        self, request: urllib.request.Request
    ) -> tuple[int, ResponseHeaders, Any]:
        try:
            with self.opener.open(request, timeout=15) as response:
                raw = response.read()
                self._assert_response_has_no_secret(raw, response.headers)
                parsed_body = json.loads(raw) if raw else None
                return (
                    response.status,
                    ResponseHeaders.from_message(response.headers),
                    parsed_body,
                )
        except urllib.error.HTTPError as error:
            raw = error.read()
            self._assert_response_has_no_secret(raw, error.headers)
            raise HttpFailure(
                error.code, ResponseHeaders.from_message(error.headers)
            ) from None

    def _assert_response_has_no_secret(self, body: bytes, headers: Any) -> None:
        header_bytes = "\n".join(
            f"{name}: {value}" for name, value in headers.items()
        ).encode()
        for value in self.forbidden_response_values:
            if value in body or value in header_bytes:
                raise RegressionFailure("HTTP response echoed a supplied secret")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RegressionFailure(message)


def expect_http_failure(
    operation: Any, allowed_statuses: set[int], message: str
) -> HttpFailure:
    try:
        operation()
    except HttpFailure as error:
        require(
            error.status in allowed_statuses,
            f"{message}: unexpected HTTP {error.status}",
        )
        return error
    raise RegressionFailure(f"{message}: request unexpectedly succeeded")


def assert_secrets_absent(value: Any, secrets: list[str], context: str) -> None:
    encoded = json.dumps(value, ensure_ascii=False, sort_keys=True)
    for secret in secrets:
        require(secret not in encoded, f"{context} echoed a supplied secret")


def provider_by_kind(providers: list[dict[str, Any]], kind: str) -> dict[str, Any]:
    matches = [provider for provider in providers if provider.get("kind") == kind]
    require(len(matches) == 1, f"expected exactly one {kind} provider")
    return matches[0]


def cancel_probe_job(client: AdminClient, job_id: str, csrf: str | None = None) -> None:
    client.request("POST", f"/api/jobs/{urllib.parse.quote(job_id)}/cancel", csrf=csrf)
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        _, _, job = client.request("GET", f"/api/jobs/{urllib.parse.quote(job_id)}")
        if job.get("status") in {"cancelled", "failed", "succeeded"}:
            return
        time.sleep(0.5)
    raise RegressionFailure("probe translation did not reach a terminal state")


def wait_for_idle_coordinator(client: AdminClient) -> None:
    deadline = time.monotonic() + ADMISSION_TIMEOUT_SECONDS
    while True:
        try:
            _, _, health = client.request("GET", "/api/health")
            if isinstance(health, dict) and health.get("activeJobs") == 0:
                return
        except HttpFailure as error:
            if error.status not in {429, 503}:
                raise
        if time.monotonic() >= deadline:
            raise RegressionFailure(
                "coordinator did not finish worker rotation and shared-volume cleanup"
            )
        time.sleep(1)


def scan_state_for_plaintext(scan_root: pathlib.Path, secrets: list[str]) -> int:
    require(scan_root.is_absolute(), "scan root must be an absolute Docker volume path")
    try:
        scan_root = scan_root.resolve(strict=True)
    except OSError as error:
        raise RegressionFailure("scan root does not exist") from error
    require(scan_root.is_dir(), f"scan root is not a directory: {scan_root}")
    require(scan_root != pathlib.Path("/"), "refusing to scan the filesystem root")
    require(
        scan_root.name == "_data" and scan_root.parent.parent.name == "volumes",
        "scan root must be a Docker named-volume _data directory",
    )
    secret_bytes = [(label, value.encode()) for label, value in enumerate(secrets)]
    overlap = max((len(value) for _, value in secret_bytes), default=1) - 1
    checked = 0
    for path in scan_root.rglob("*"):
        if path.is_symlink():
            raise RegressionFailure(f"state scan refused symbolic link: {path}")
        if path.is_dir():
            continue
        descriptor = os.open(
            path,
            os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK,
        )
        try:
            metadata = os.fstat(descriptor)
            require(
                stat.S_ISREG(metadata.st_mode),
                f"state scan refused non-regular file: {path}",
            )
            checked += 1
            tail = b""
            with os.fdopen(descriptor, "rb", closefd=False) as source:
                while chunk := source.read(1024 * 1024):
                    content = tail + chunk
                    for label, value in secret_bytes:
                        require(
                            value not in content,
                            f"plaintext secret #{label + 1} found in state",
                        )
                    tail = content[-overlap:] if overlap else b""
        finally:
            os.close(descriptor)
    return checked


def scan_container_logs_for_plaintext(compose_project: str, secrets: list[str]) -> None:
    require(bool(compose_project.strip()), "Compose project name is blank")
    listed = subprocess.run(  # noqa: S603 - fixed executable and argument list
        [
            "/usr/bin/docker",
            "ps",
            "-a",
            "--filter",
            f"label=com.docker.compose.project={compose_project}",
            "--format",
            "{{.ID}}",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    container_ids = [value for value in listed.stdout.splitlines() if value]
    require(bool(container_ids), "log scan found no Compose containers")
    secret_bytes = [value.encode() for value in secrets]
    for container_id in container_ids:
        captured = subprocess.run(  # noqa: S603 - validated Docker container ID
            ["/usr/bin/docker", "logs", container_id],
            check=False,
            capture_output=True,
        )
        require(
            captured.returncode == 0,
            f"could not read logs for Compose container {container_id}",
        )
        logs = captured.stdout + captured.stderr
        for index, value in enumerate(secret_bytes, start=1):
            require(value not in logs, f"plaintext secret #{index} found in logs")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("base_url")
    parser.add_argument("--setup-token-file", type=pathlib.Path, required=True)
    parser.add_argument("--scan-root", type=pathlib.Path, action="append", default=[])
    parser.add_argument("--compose-project")
    parser.add_argument(
        "--allow-non-loopback",
        action="store_true",
        help="allow destructive first-admin checks against a non-loopback URL",
    )
    args = parser.parse_args()

    parsed_base = urllib.parse.urlsplit(args.base_url)
    host = parsed_base.hostname or ""
    try:
        loopback = ipaddress.ip_address(host).is_loopback
    except ValueError:
        loopback = host.lower() == "localhost"
    require(
        loopback or args.allow_non_loopback,
        "refusing destructive admin setup checks against a non-loopback URL",
    )
    setup_token = args.setup_token_file.read_text(encoding="utf-8").strip()
    require(bool(setup_token), "setup token file is blank")

    run_id = str(int(time.time() * 1000))
    usernames = [f"e2e-admin-a-{run_id}", f"e2e-admin-b-{run_id}"]
    passwords = [
        f"E2E-only-A-{run_id}-Strong!",
        f"E2E-only-B-{run_id}-Strong!",
    ]
    deepseek_key = f"sk-e2e-deepseek-{run_id}-do-not-use"
    compatible_key = f"sk-e2e-compatible-{run_id}-do-not-use"
    supplied_secrets = [
        setup_token,
        *passwords,
        deepseek_key,
        compatible_key,
    ]

    def new_client() -> AdminClient:
        return AdminClient(args.base_url, supplied_secrets)

    anonymous = new_client()
    status, headers, session = anonymous.request("GET", "/api/admin/session")
    require(status == 200, "admin session endpoint is unavailable")
    require(session.get("setupRequired") is True, "disposable state is already set up")
    require(
        session.get("setupTokenRequired") is True,
        "setup token is not required on the disposable deployment",
    )
    require(session.get("authenticated") is False, "anonymous session is authenticated")
    assert_secrets_absent(session, supplied_secrets, "session response")
    require(
        "no-store" in headers.get("Cache-Control", "").lower(),
        "admin session is cacheable",
    )

    expect_http_failure(
        lambda: anonymous.request("GET", "/api/admin/providers"),
        {401, 403},
        "anonymous provider read",
    )
    expect_http_failure(
        lambda: new_client().request(
            "POST",
            "/api/admin/setup",
            {
                "setupToken": f"wrong-{run_id}",
                "username": f"wrong-{run_id}",
                "password": passwords[0],
            },
        ),
        {401, 403},
        "wrong setup token",
    )

    setup_clients = [new_client(), new_client()]

    def race_setup(index: int) -> tuple[int, ResponseHeaders, Any] | HttpFailure:
        try:
            return setup_clients[index].request(
                "POST",
                "/api/admin/setup",
                {
                    "setupToken": setup_token,
                    "username": usernames[index],
                    "password": passwords[index],
                },
            )
        except HttpFailure as error:
            return error

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        outcomes = list(executor.map(race_setup, range(2)))

    winners = [
        index
        for index, outcome in enumerate(outcomes)
        if not isinstance(outcome, HttpFailure)
    ]
    require(len(winners) == 1, "first-admin setup was not atomic")
    winner = winners[0]
    loser = 1 - winner
    losing_outcome = outcomes[loser]
    require(isinstance(losing_outcome, HttpFailure), "losing setup did not fail")
    require(losing_outcome.status == 409, "losing setup returned the wrong status")

    admin = setup_clients[winner]
    setup_status, setup_headers, admin_session = outcomes[winner]  # type: ignore[misc]
    require(setup_status == 201, "winning setup did not return HTTP 201")
    require(admin_session.get("authenticated") is True, "setup did not log in admin")
    csrf = admin_session.get("csrfToken")
    require(isinstance(csrf, str) and len(csrf) >= 32, "setup returned no CSRF token")
    response_cookies = setup_headers.get_all("Set-Cookie")
    session_cookies = [
        cookie
        for cookie in response_cookies
        if cookie.lower().startswith(("pdf2zh_session=", "__host-pdf2zh_session="))
    ]
    csrf_cookies = [
        cookie
        for cookie in response_cookies
        if cookie.lower().startswith(("pdf2zh_csrf=", "__host-pdf2zh_csrf="))
    ]
    require(len(session_cookies) == 1, "setup did not set exactly one session cookie")
    require(len(csrf_cookies) == 1, "setup did not set exactly one CSRF cookie")
    session_cookie = session_cookies[0].lower()
    csrf_cookie = csrf_cookies[0].lower()
    require("httponly" in session_cookie, "session cookie lacks HttpOnly")
    require("samesite=strict" in session_cookie, "session cookie lacks SameSite=Strict")
    require("samesite=strict" in csrf_cookie, "CSRF cookie lacks SameSite=Strict")
    if parsed_base.scheme.lower() == "https":
        require("; secure" in session_cookie, "HTTPS session cookie lacks Secure")
        require("; secure" in csrf_cookie, "HTTPS CSRF cookie lacks Secure")
        require(
            session_cookie.startswith("__host-pdf2zh_session="),
            "HTTPS session cookie lacks __Host- prefix",
        )
        require(
            csrf_cookie.startswith("__host-pdf2zh_csrf="),
            "HTTPS CSRF cookie lacks __Host- prefix",
        )
    assert_secrets_absent(admin_session, supplied_secrets, "setup response")

    expect_http_failure(
        lambda: new_client().request(
            "POST",
            "/api/admin/login",
            {"username": usernames[loser], "password": passwords[loser]},
        ),
        {401, 429},
        "losing setup user can log in",
    )

    status, _, provider_payload = admin.request("GET", "/api/admin/providers")
    require(status == 200, "provider list failed")
    providers = provider_payload.get("providers", [])
    deepseek = provider_by_kind(providers, "deepseek")
    require(deepseek.get("hasApiKey") is False, "DeepSeek unexpectedly has a key")
    require(deepseek.get("enabled") is False, "DeepSeek unexpectedly starts enabled")

    deepseek_write = {
        "kind": "deepseek",
        "name": "DeepSeek",
        "baseUrl": "https://api.deepseek.com",
        "enabled": True,
        "apiKey": deepseek_key,
        "clearApiKey": False,
        "expectedRevision": deepseek["revision"],
        "models": [
            {
                "id": "deepseek-v4-flash",
                "name": "DeepSeek V4 Flash",
                "isDefault": True,
            },
            {
                "id": "deepseek-v4-pro",
                "name": "DeepSeek V4 Pro",
                "isDefault": False,
            },
        ],
    }
    expect_http_failure(
        lambda: admin.request("PUT", "/api/admin/providers/deepseek", deepseek_write),
        {403},
        "provider write without CSRF",
    )
    expect_http_failure(
        lambda: admin.request(
            "PUT",
            "/api/admin/providers/deepseek",
            deepseek_write,
            csrf=csrf,
            origin="https://wrong-origin.invalid",
        ),
        {403},
        "provider write with a foreign Origin",
    )
    _, _, deepseek = admin.request(
        "PUT", "/api/admin/providers/deepseek", deepseek_write, csrf=csrf
    )
    require(deepseek.get("hasApiKey") is True, "DeepSeek key was not stored")
    require(deepseek.get("enabled") is True, "DeepSeek was not enabled")
    assert_secrets_absent(deepseek, supplied_secrets, "DeepSeek response")

    _, _, public_config = anonymous.request("GET", "/api/config")
    require(
        {provider.get("id") for provider in public_config.get("providers", [])}
        == {"google"},
        "anonymous config exposed a credentialed provider",
    )
    require("csrfToken" not in public_config, "anonymous config exposed a CSRF token")
    _, _, administrator_config = admin.request("GET", "/api/config")
    administrator_deepseek = provider_by_kind(
        administrator_config.get("providers", []), "deepseek"
    )
    require(
        administrator_deepseek.get("available") is True,
        "DeepSeek is unavailable to the administrator after enable",
    )
    require(
        {model["id"] for model in administrator_deepseek.get("models", [])}
        == {"deepseek-v4-flash", "deepseek-v4-pro"},
        "DeepSeek exposes obsolete or missing models",
    )
    require(
        administrator_config.get("csrfToken") == csrf,
        "administrator config did not return its CSRF token",
    )
    assert_secrets_absent(public_config, supplied_secrets, "public config")
    assert_secrets_absent(
        administrator_config, supplied_secrets, "administrator config"
    )

    expect_http_failure(
        lambda: anonymous.submit_probe_job("deepseek", model_id="deepseek-v4-flash"),
        {401},
        "anonymous credentialed translation",
    )
    expect_http_failure(
        lambda: admin.submit_probe_job("deepseek", model_id="deepseek-v4-flash"),
        {403},
        "administrator translation without CSRF",
    )
    expect_http_failure(
        lambda: admin.submit_probe_job(
            "deepseek",
            model_id="deepseek-v4-flash",
            csrf=csrf,
            origin="https://wrong-origin.invalid",
        ),
        {403},
        "administrator translation with foreign Origin",
    )
    paid_status, _, paid_job = admin.submit_probe_job(
        "deepseek", model_id="deepseek-v4-flash", csrf=csrf
    )
    require(paid_status in {200, 201, 202}, "administrator paid translation failed")
    require(isinstance(paid_job.get("id"), str), "paid translation returned no job ID")
    cancel_probe_job(admin, paid_job["id"], csrf)

    google_status, _, google_job = anonymous.submit_probe_job("google")
    require(google_status in {200, 201, 202}, "anonymous Google translation failed")
    require(
        isinstance(google_job.get("id"), str), "Google translation returned no job ID"
    )
    cancel_probe_job(anonymous, google_job["id"])
    wait_for_idle_coordinator(anonymous)

    clear_deepseek = {
        **deepseek_write,
        "enabled": False,
        "clearApiKey": True,
        "expectedRevision": deepseek["revision"],
    }
    clear_deepseek.pop("apiKey")
    _, _, cleared = admin.request(
        "PUT", "/api/admin/providers/deepseek", clear_deepseek, csrf=csrf
    )
    require(
        cleared.get("hasApiKey") is False, "clearApiKey did not remove DeepSeek key"
    )
    require(cleared.get("enabled") is False, "keyless DeepSeek remained enabled")
    deepseek_write["expectedRevision"] = cleared["revision"]
    _, _, deepseek = admin.request(
        "PUT", "/api/admin/providers/deepseek", deepseek_write, csrf=csrf
    )
    require(deepseek.get("hasApiKey") is True, "DeepSeek key could not be restored")
    require(deepseek.get("enabled") is True, "DeepSeek could not be re-enabled")

    generic_write = {
        "kind": "openai_compatible",
        "name": "Disposable compatible provider",
        "baseUrl": "https://api.openai.com/v1",
        "enabled": True,
        "apiKey": compatible_key,
        "clearApiKey": False,
        "models": [
            {"id": "e2e-model-a", "name": "E2E model A", "isDefault": True},
            {"id": "e2e-model-b", "name": "E2E model B", "isDefault": False},
        ],
    }
    _, _, generic = admin.request(
        "POST", "/api/admin/providers", generic_write, csrf=csrf
    )
    generic_id = generic.get("id")
    require(isinstance(generic_id, str) and generic_id, "generic provider has no ID")
    require(generic.get("hasApiKey") is True, "generic provider key was not stored")
    assert_secrets_absent(generic, supplied_secrets, "generic create response")

    original_revision = generic["revision"]
    updated_write = {
        **generic_write,
        "name": "Updated compatible provider",
        "expectedRevision": original_revision,
    }
    updated_write.pop("apiKey")
    _, _, generic = admin.request(
        "PUT",
        f"/api/admin/providers/{urllib.parse.quote(generic_id)}",
        updated_write,
        csrf=csrf,
    )
    require(generic.get("hasApiKey") is True, "omitting key cleared existing key")
    require(
        generic["revision"] > original_revision, "provider revision did not advance"
    )

    expect_http_failure(
        lambda: admin.request(
            "PUT",
            f"/api/admin/providers/{urllib.parse.quote(generic_id)}",
            updated_write,
            csrf=csrf,
        ),
        {409},
        "stale provider revision",
    )

    _, _, public_config = anonymous.request("GET", "/api/config")
    public_ids = {provider.get("id") for provider in public_config.get("providers", [])}
    require(generic_id not in public_ids, "anonymous config exposed generic provider")
    _, _, administrator_config = admin.request("GET", "/api/config")
    administrator_ids = {
        provider.get("id") for provider in administrator_config.get("providers", [])
    }
    require(generic_id in administrator_ids, "generic provider is missing for admin")
    assert_secrets_absent(public_config, supplied_secrets, "updated public config")
    assert_secrets_absent(
        administrator_config, supplied_secrets, "updated administrator config"
    )

    admin.request(
        "DELETE",
        f"/api/admin/providers/{urllib.parse.quote(generic_id)}?"
        + urllib.parse.urlencode({"expectedRevision": generic["revision"]}),
        csrf=csrf,
    )
    _, _, administrator_config = admin.request("GET", "/api/config")
    administrator_ids = {
        provider.get("id") for provider in administrator_config.get("providers", [])
    }
    require(generic_id not in administrator_ids, "deleted provider remains selectable")

    admin.request("POST", "/api/admin/logout", csrf=csrf)
    _, _, logged_out = admin.request("GET", "/api/admin/session")
    require(logged_out.get("authenticated") is False, "logout did not revoke session")
    expect_http_failure(
        lambda: admin.request("GET", "/api/admin/providers"),
        {401, 403},
        "revoked session still reads providers",
    )

    login = new_client()
    expect_http_failure(
        lambda: login.request(
            "POST",
            "/api/admin/login",
            {"username": usernames[winner], "password": f"wrong-{run_id}"},
        ),
        {401, 429},
        "wrong admin password",
    )
    _, _, logged_in = login.request(
        "POST",
        "/api/admin/login",
        {"username": usernames[winner], "password": passwords[winner]},
    )
    require(logged_in.get("authenticated") is True, "valid admin login failed")
    assert_secrets_absent(logged_in, supplied_secrets, "login response")

    scanned_files = sum(
        scan_state_for_plaintext(
            scan_root,
            [*passwords, deepseek_key, compatible_key],
        )
        for scan_root in args.scan_root
    )
    if args.scan_root:
        require(scanned_files > 0, "state scans did not inspect any files")
    if args.compose_project:
        scan_container_logs_for_plaintext(args.compose_project, supplied_secrets)

    checks = [
        "atomic_setup",
        "session_cookie",
        "origin_csrf",
        "deepseek_v4",
        "credentialed_provider_auth",
        "encrypted_provider_keys",
        "generic_provider_crud",
        "optimistic_revision",
        "logout_login",
    ]
    if args.scan_root:
        checks.append("secret_free_volumes")
    if args.compose_project:
        checks.append("secret_free_logs")
    print(
        json.dumps(
            {
                "ok": True,
                "checks": checks,
            },
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
