#!/usr/bin/env python3
"""Black-box admin security regression that never submits a translation job.

Run only against a disposable deployment with an unclaimed administrator. The
HTTP and HTTPS URLs must reach the same deployment and use the same hostname.
The HTTPS endpoint may be a temporary, user-managed reverse proxy.
"""

from __future__ import annotations

import argparse
import http.client
import http.cookiejar
import ipaddress
import json
import pathlib
import ssl
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any


class RegressionFailure(RuntimeError):  # noqa: N818 - regression script contract
    pass


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
        values = self.values.get(name.lower())
        return ", ".join(values) if values else default

    def get_all(self, name: str) -> list[str]:
        return list(self.values.get(name.lower(), []))


@dataclass(slots=True)
class HttpFailure(Exception):  # noqa: N818 - carries an HTTP status for assertions
    status: int
    headers: ResponseHeaders


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RegressionFailure(message)


def assert_no_secret(raw: bytes, headers: Any, secrets: list[bytes]) -> None:
    header_bytes = "\n".join(
        f"{name}: {value}" for name, value in headers.items()
    ).encode()
    for index, secret in enumerate(secrets, start=1):
        if secret in raw or secret in header_bytes:
            raise RegressionFailure(
                f"HTTP response exposed supplied secret #{index}"
            )


class AdminClient:
    def __init__(
        self,
        base_url: str,
        secrets: list[bytes],
        *,
        cookie_jar: http.cookiejar.CookieJar | None = None,
        insecure_tls: bool = False,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        parsed = urllib.parse.urlsplit(self.base_url)
        self.origin = f"{parsed.scheme}://{parsed.netloc}"
        self.secrets = secrets
        self.cookies = cookie_jar or http.cookiejar.CookieJar()
        handlers: list[Any] = [
            urllib.request.ProxyHandler({}),
            urllib.request.HTTPCookieProcessor(self.cookies),
        ]
        if parsed.scheme == "https":
            context = ssl.create_default_context()
            if insecure_tls:
                context.check_hostname = False
                context.verify_mode = ssl.CERT_NONE  # noqa: S323 - disposable TLS proxy
            handlers.append(urllib.request.HTTPSHandler(context=context))
        self.opener = urllib.request.build_opener(*handlers)

    def request(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None = None,
        *,
        csrf: str | None = None,
        origin: str | None = "default",
        host: str | None = None,
        fetch_site: str | None = "same-origin",
    ) -> tuple[int, ResponseHeaders, Any]:
        body = None
        headers = {
            "Accept": "application/json",
            "X-PDF2ZH-Origin": self.origin,
        }
        if payload is not None:
            body = json.dumps(payload, separators=(",", ":")).encode()
            headers["Content-Type"] = "application/json"
        if method not in {"GET", "HEAD"} and origin is not None:
            headers["Origin"] = self.origin if origin == "default" else origin
        if host is not None:
            headers["Host"] = host
        if fetch_site is not None:
            headers["Sec-Fetch-Site"] = fetch_site
        if csrf is not None:
            headers["X-CSRF-Token"] = csrf
        request = urllib.request.Request(  # noqa: S310 - validated HTTP(S) URLs
            f"{self.base_url}{path}",
            data=body,
            headers=headers,
            method=method,
        )
        return self._open(request)

    def _open(
        self, request: urllib.request.Request
    ) -> tuple[int, ResponseHeaders, Any]:
        try:
            with self.opener.open(request, timeout=20) as response:
                raw = response.read()
                assert_no_secret(raw, response.headers, self.secrets)
                return (
                    response.status,
                    ResponseHeaders.from_message(response.headers),
                    json.loads(raw) if raw else None,
                )
        except urllib.error.HTTPError as error:
            raw = error.read()
            assert_no_secret(raw, error.headers, self.secrets)
            raise HttpFailure(
                error.code, ResponseHeaders.from_message(error.headers)
            ) from None


def expect_failure(operation: Any, statuses: set[int], message: str) -> None:
    try:
        operation()
    except HttpFailure as error:
        require(error.status in statuses, f"{message}: unexpected HTTP {error.status}")
        return
    raise RegressionFailure(f"{message}: request unexpectedly succeeded")


def validate_endpoint(value: str, expected_scheme: str) -> urllib.parse.SplitResult:
    parsed = urllib.parse.urlsplit(value)
    require(parsed.scheme == expected_scheme, f"expected an {expected_scheme} URL")
    require(bool(parsed.hostname), "endpoint URL has no hostname")
    require(parsed.username is None and parsed.password is None, "URL contains credentials")
    require(parsed.path in {"", "/"}, "endpoint URL must not contain a path")
    require(not parsed.query and not parsed.fragment, "endpoint URL has query or fragment")
    return parsed


def is_loopback(hostname: str) -> bool:
    try:
        return ipaddress.ip_address(hostname).is_loopback
    except ValueError:
        return hostname.lower() == "localhost"


def raw_duplicate_header_probe(
    parsed: urllib.parse.SplitResult,
    setup_payload: dict[str, Any],
    duplicate_name: str,
    secrets: list[bytes],
) -> int:
    body = json.dumps(setup_payload, separators=(",", ":")).encode()
    connection = http.client.HTTPConnection(
        parsed.hostname,
        parsed.port or 80,
        timeout=20,
    )
    try:
        connection.putrequest("POST", "/api/admin/setup", skip_host=True)
        if duplicate_name == "Host":
            connection.putheader("Host", parsed.netloc)
            connection.putheader("Host", parsed.netloc)
        else:
            connection.putheader("Host", parsed.netloc)
        origin = f"http://{parsed.netloc}"
        connection.putheader("Origin", origin)
        if duplicate_name == "Origin":
            connection.putheader("Origin", origin)
        connection.putheader("Sec-Fetch-Site", "same-origin")
        connection.putheader("Content-Type", "application/json")
        connection.putheader("Content-Length", str(len(body)))
        connection.endheaders(body)
        response = connection.getresponse()
        raw = response.read()
        assert_no_secret(raw, response.headers, secrets)
        return response.status
    finally:
        connection.close()


def assert_cookie_pair(headers: ResponseHeaders, *, secure: bool) -> None:
    cookies = headers.get_all("Set-Cookie")
    session_name = "__host-pdf2zh_session=" if secure else "pdf2zh_session="
    csrf_name = "__host-pdf2zh_csrf=" if secure else "pdf2zh_csrf="
    session = [item for item in cookies if item.lower().startswith(session_name)]
    csrf = [item for item in cookies if item.lower().startswith(csrf_name)]
    require(len(session) == 1, "response did not set exactly one session cookie")
    require(len(csrf) == 1, "response did not set exactly one CSRF cookie")
    session_text = session[0].lower()
    csrf_text = csrf[0].lower()
    require("path=/" in session_text and "path=/" in csrf_text, "cookie path is wrong")
    require("domain=" not in session_text + csrf_text, "cookie unexpectedly has Domain")
    require("httponly" in session_text, "session cookie lacks HttpOnly")
    require("httponly" not in csrf_text, "CSRF cookie unexpectedly has HttpOnly")
    require(
        "samesite=strict" in session_text and "samesite=strict" in csrf_text,
        "admin cookies lack SameSite=Strict",
    )
    if secure:
        require("; secure" in session_text, "HTTPS session cookie lacks Secure")
        require("; secure" in csrf_text, "HTTPS CSRF cookie lacks Secure")
    else:
        require("; secure" not in session_text, "HTTP session cookie has Secure")
        require("; secure" not in csrf_text, "HTTP CSRF cookie has Secure")


def provider_by_kind(providers: list[dict[str, Any]], kind: str) -> dict[str, Any]:
    matches = [item for item in providers if item.get("kind") == kind]
    require(len(matches) == 1, f"expected exactly one {kind} provider")
    return matches[0]


def deepseek_payload(
    revision: int,
    *,
    api_key: str | None = None,
    clear_api_key: bool = False,
    enabled: bool = True,
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "kind": "deepseek",
        "name": "DeepSeek",
        "baseUrl": "https://api.deepseek.com",
        "enabled": enabled,
        "clearApiKey": clear_api_key,
        "expectedRevision": revision,
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
    if api_key is not None:
        payload["apiKey"] = api_key
    return payload


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("http_base_url")
    parser.add_argument("https_base_url")
    parser.add_argument("--setup-token-file", type=pathlib.Path, required=True)
    parser.add_argument(
        "--insecure-tls",
        action="store_true",
        help="accept the disposable HTTPS proxy's self-signed certificate",
    )
    parser.add_argument(
        "--allow-non-loopback",
        action="store_true",
        help="allow destructive first-admin checks against non-loopback URLs",
    )
    args = parser.parse_args()

    http_endpoint = validate_endpoint(args.http_base_url, "http")
    https_endpoint = validate_endpoint(args.https_base_url, "https")
    require(
        http_endpoint.hostname.lower() == https_endpoint.hostname.lower(),
        "HTTP and HTTPS endpoints must use the same hostname",
    )
    if not args.allow_non_loopback:
        require(
            is_loopback(http_endpoint.hostname),
            "refusing first-admin checks against a non-loopback deployment",
        )

    setup_token = args.setup_token_file.read_text(encoding="utf-8").strip()
    require(bool(setup_token), "setup token file is blank")
    run_id = str(int(time.time() * 1000))
    username = f"e2e-admin-{run_id}"
    password = f"E2E-only-{run_id}-Strong!"
    deepseek_key = f"sk-e2e-deepseek-{run_id}-never-send"
    compatible_key = f"sk-e2e-compatible-{run_id}-never-send"
    secret_values = [setup_token, password, deepseek_key, compatible_key]
    secrets = [value.encode() for value in secret_values]

    setup_payload = {
        "setupToken": setup_token,
        "username": username,
        "password": password,
    }
    http_client = AdminClient(args.http_base_url, secrets)
    status, headers, session = http_client.request("GET", "/api/admin/session")
    require(status == 200, "admin session endpoint is unavailable")
    require(session.get("setupRequired") is True, "deployment is already set up")
    require(
        session.get("setupTokenRequired") is True,
        "disposable deployment does not require its setup token",
    )
    require(session.get("authenticated") is False, "anonymous session is authenticated")
    require("no-store" in headers.get("Cache-Control").lower(), "session is cacheable")
    expect_failure(
        lambda: http_client.request("GET", "/api/admin/providers"),
        {401, 403},
        "anonymous provider read",
    )

    expect_failure(
        lambda: http_client.request(
            "POST", "/api/admin/setup", setup_payload, origin=None
        ),
        {403},
        "setup without Origin",
    )
    expect_failure(
        lambda: http_client.request(
            "POST",
            "/api/admin/setup",
            setup_payload,
            origin="https://foreign.example.invalid",
        ),
        {403},
        "setup with foreign Origin",
    )
    expect_failure(
        lambda: http_client.request(
            "POST",
            "/api/admin/setup",
            setup_payload,
            host="foreign.example.invalid",
        ),
        {403},
        "setup with mismatched Host",
    )
    expect_failure(
        lambda: http_client.request(
            "POST",
            "/api/admin/setup",
            setup_payload,
            fetch_site="cross-site",
        ),
        {403},
        "cross-site setup",
    )
    require(
        raw_duplicate_header_probe(
            http_endpoint, setup_payload, "Origin", secrets
        )
        in {400, 403},
        "duplicate Origin was not rejected",
    )
    require(
        raw_duplicate_header_probe(http_endpoint, setup_payload, "Host", secrets)
        in {400, 403},
        "duplicate Host was not rejected",
    )

    status, setup_headers, created = http_client.request(
        "POST", "/api/admin/setup", setup_payload
    )
    require(status == 201, "first administrator setup did not return HTTP 201")
    require(created.get("authenticated") is True, "setup did not authenticate admin")
    http_csrf = created.get("csrfToken")
    require(isinstance(http_csrf, str) and len(http_csrf) >= 32, "setup has no CSRF")
    assert_cookie_pair(setup_headers, secure=False)
    expect_failure(
        lambda: AdminClient(args.http_base_url, secrets).request(
            "POST",
            "/api/admin/setup",
            {
                "setupToken": setup_token,
                "username": f"second-{run_id}",
                "password": password,
            },
        ),
        {409},
        "second administrator setup",
    )
    _, _, http_session = http_client.request("GET", "/api/admin/session")
    require(http_session.get("authenticated") is True, "HTTP cookie login failed")

    # Share the browser jar to exercise an HTTP-to-HTTPS migration where both
    # cookie families coexist. X-PDF2ZH-Origin must select the secure family.
    https_client = AdminClient(
        args.https_base_url,
        secrets,
        cookie_jar=http_client.cookies,
        insecure_tls=args.insecure_tls,
    )
    status, login_headers, logged_in = https_client.request(
        "POST",
        "/api/admin/login",
        {"username": username, "password": password},
    )
    require(status == 200, "HTTPS administrator login failed")
    require(logged_in.get("authenticated") is True, "HTTPS login is not authenticated")
    https_csrf = logged_in.get("csrfToken")
    require(isinstance(https_csrf, str) and len(https_csrf) >= 32, "login has no CSRF")
    assert_cookie_pair(login_headers, secure=True)
    _, _, https_session = https_client.request("GET", "/api/admin/session")
    require(https_session.get("authenticated") is True, "HTTPS cookie login failed")
    _, _, migrated_http_session = http_client.request("GET", "/api/admin/session")
    require(
        migrated_http_session.get("authenticated") is True,
        "secure cookies shadowed the HTTP cookie family",
    )

    _, _, provider_list = https_client.request("GET", "/api/admin/providers")
    deepseek = provider_by_kind(provider_list.get("providers", []), "deepseek")
    require(deepseek.get("hasApiKey") is False, "DeepSeek unexpectedly has a key")
    write_deepseek = deepseek_payload(
        deepseek["revision"], api_key=deepseek_key, enabled=True
    )
    expect_failure(
        lambda: https_client.request(
            "PUT", "/api/admin/providers/deepseek", write_deepseek
        ),
        {403},
        "DeepSeek write without CSRF",
    )
    expect_failure(
        lambda: https_client.request(
            "PUT",
            "/api/admin/providers/deepseek",
            write_deepseek,
            csrf=https_csrf,
            origin="https://foreign.example.invalid",
        ),
        {403},
        "DeepSeek write with foreign Origin",
    )
    expect_failure(
        lambda: https_client.request(
            "PUT",
            "/api/admin/providers/deepseek",
            write_deepseek,
            csrf=https_csrf,
            origin=None,
        ),
        {403},
        "DeepSeek write without Origin",
    )
    expect_failure(
        lambda: https_client.request(
            "PUT",
            "/api/admin/providers/deepseek",
            write_deepseek,
            csrf=https_csrf,
            host="foreign.example.invalid",
        ),
        {401, 403},
        "DeepSeek write with mismatched Host",
    )
    _, _, deepseek = https_client.request(
        "PUT",
        "/api/admin/providers/deepseek",
        write_deepseek,
        csrf=https_csrf,
    )
    require(deepseek.get("hasApiKey") is True, "DeepSeek key was not stored")
    require(deepseek.get("enabled") is True, "DeepSeek was not enabled")

    anonymous = AdminClient(args.https_base_url, secrets, insecure_tls=args.insecure_tls)
    anonymous.request("GET", "/api/config")
    https_client.request("GET", "/api/config")
    _, _, providers_after_write = https_client.request("GET", "/api/admin/providers")
    listed_deepseek = provider_by_kind(
        providers_after_write.get("providers", []), "deepseek"
    )
    require(listed_deepseek.get("hasApiKey") is True, "DeepSeek key flag was lost")

    compatible_write = {
        "kind": "openai_compatible",
        "name": "Disposable no-paid provider",
        "baseUrl": "https://api.example.invalid/v1",
        "enabled": True,
        "apiKey": compatible_key,
        "clearApiKey": False,
        "models": [
            {"id": "no-paid-model", "name": "No-paid model", "isDefault": True}
        ],
    }
    status, _, compatible = https_client.request(
        "POST",
        "/api/admin/providers",
        compatible_write,
        csrf=https_csrf,
    )
    require(status == 201, "OpenAI-compatible provider creation failed")
    compatible_id = compatible.get("id")
    require(isinstance(compatible_id, str) and compatible_id, "provider has no ID")
    require(compatible.get("hasApiKey") is True, "compatible key was not stored")
    https_client.request("GET", "/api/admin/providers")
    https_client.request("GET", "/api/config")
    anonymous.request("GET", "/api/config")

    https_client.request(
        "DELETE",
        f"/api/admin/providers/{urllib.parse.quote(compatible_id)}?"
        + urllib.parse.urlencode({"expectedRevision": compatible["revision"]}),
        csrf=https_csrf,
    )
    _, _, cleared = https_client.request(
        "PUT",
        "/api/admin/providers/deepseek",
        deepseek_payload(
            deepseek["revision"], clear_api_key=True, enabled=False
        ),
        csrf=https_csrf,
    )
    require(cleared.get("hasApiKey") is False, "DeepSeek key was not cleared")
    require(cleared.get("enabled") is False, "cleared DeepSeek remains enabled")

    https_client.request("POST", "/api/admin/logout", csrf=https_csrf)
    _, _, after_https_logout = https_client.request("GET", "/api/admin/session")
    require(
        after_https_logout.get("authenticated") is False,
        "HTTPS logout did not revoke its session",
    )
    _, _, after_http_logout = http_client.request("GET", "/api/admin/session")
    require(
        after_http_logout.get("authenticated") is False,
        "logout did not clear the HTTP cookie family",
    )

    print(
        json.dumps(
            {
                "ok": True,
                "paidTranslationRequests": 0,
                "checks": [
                    "first_admin_setup",
                    "origin_host_rejection",
                    "duplicate_header_rejection",
                    "http_cookie_family",
                    "https_cookie_family",
                    "http_https_cookie_migration",
                    "deepseek_v4_configuration",
                    "openai_compatible_configuration",
                    "credential_non_disclosure",
                    "provider_cleanup",
                    "logout",
                ],
            },
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
