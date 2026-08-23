# Rust coordinator and isolated translation worker

The server deployment consists of three containers and publishes one ordinary
HTTP port:

```text
browser ── HTTP :8199 ──> Rust API + React frontend
                              │ private worker network
                              ▼
                         FastAPI worker

one-shot init ──> data/ + secrets/
```

- `web` is the public Rust coordinator. It owns authentication, provider
  configuration, uploads, the job state machine, artifacts, leases, and the
  React application. Its container port is 7860 and Compose publishes it as
  host port 8199.
- `translator` is the private Python/FastAPI BabelDOC bridge. It is reachable
  only from `web` on the Compose network and always runs one translation at a
  time.
- `init` is a network-disabled one-shot container. It creates and validates
  the persistent directories, provider master key, administrator setup token,
  and matching web/worker authentication tokens.

TLS termination and reverse proxying are deliberately outside this project.
There is no gateway, Certbot service, server-IP setting, domain setting, or
certificate directory in the supported Compose deployment.

## Request and job lifecycle

The browser reserves a UUID before upload and sends it as `X-Job-Id`. The Rust
coordinator persists the job before starting the worker request. Admission is
fixed to one active job, and retries reuse the same UUID and multipart body so
a lost admission response cannot create a duplicate job.

The normal state progression is:

```text
uploading → queued → running → succeeded
                         └──→ cancelling → cancelled
                         └──→ failed
```

The coordinator owns the cancellation token, task handle, worker epoch, and
the single admission permit. A cancel request is idempotent. It signals the
worker, waits for termination, reconciles the final state, and releases the
permit. If the worker cannot prove that the translation process group is gone,
the worker container exits and Docker restarts it; the next job is not admitted
against an uncertain worker generation.

The browser sends heartbeats while it owns a job. Page exit sends a best-effort
release request and stops heartbeats. A short release grace period handles
reloads; an abandoned lease eventually cancels the job. A Rust process restart
reconciles every persisted active job to a terminal state before accepting new
work, so an interrupted job cannot permanently occupy the queue.

## Provider boundary

Anonymous users can submit only the built-in Google provider. Google requires
no secret and is the only provider exposed by the anonymous configuration API.

The first administrator is claimed at `/admin` with the one-time token stored
in `secrets/web/admin-setup-token`. The administrator may configure:

- DeepSeek at the current official `https://api.deepseek.com` endpoint with
  `deepseek-v4-flash` and `deepseek-v4-pro`;
- additional HTTPS OpenAI-compatible endpoints and explicit model allow-lists.

Provider keys are encrypted in SQLite with the master key from
`secrets/web/provider-master-key`. The browser never receives stored keys.
Paid-provider submission requires an authenticated administrator session plus
the CSRF cookie/header pair. The selected credential is sealed for that job in
memory and is never copied into job history.

The master key and SQLite state are one recovery unit. Startup fails closed if
state exists but the key is missing, invalid, or cannot decrypt every live
credential.

## Origin and cookie security

No public IP or domain is configured by default. For every administrator write,
the backend requires exactly one browser `Origin`, parses it as an HTTP(S)
origin, and compares its normalized host and effective port with the original
`Host` header. Missing, repeated, malformed, foreign-host, or wrong-port values
are rejected. `Forwarded`, `X-Forwarded-Host`, and `X-Forwarded-Proto` do not
participate in this decision.

Cookie transport follows the validated browser origin:

- HTTP uses `pdf2zh_session` and `pdf2zh_csrf`;
- HTTPS uses `__Host-pdf2zh_session` and `__Host-pdf2zh_csrf` with `Secure`.

Both use `Path=/` and `SameSite=Strict`; the session cookie is also `HttpOnly`.
The frontend sends an origin hint on authenticated reads so moving the same
host from HTTP to an HTTPS reverse proxy cannot leave an old insecure session
apparently logged in while every write fails. The hint is never accepted in
place of the browser-controlled `Origin` on a state-changing request.

`PDF2ZH_PUBLIC_ORIGIN` remains an optional strict origin pin for specialized
deployments. It is not needed by the distributed Compose file.

`PDF2ZH_TRUST_X_FORWARDED_FOR` defaults to false. Enabling it while port 8199 is
public would let clients forge their rate-limit source. Operators who require
forwarded client addresses must first restrict the backend port to a trusted
proxy path.

## Portable Compose deployment

Copy the complete `compose.yml` from the repository README into an empty
directory and run:

```bash
docker compose up -d --pull always --remove-orphans --wait --wait-timeout 600
```

The target server pulls three multi-architecture GHCR images and does not need
the source tree or a compiler. The default endpoint is:

```text
http://SERVER_IP:8199
```

The portable unit contains only:

```text
compose.yml
data/
secrets/
```

`data/jobs` holds inputs, translated outputs, and worker journals. `data/state`
holds SQLite sessions, providers, and job metadata. `secrets/web` and
`secrets/worker` hold the master/setup/worker secrets with restrictive
ownership and modes. Never run
`docker compose down -v`, and always migrate `data` and `secrets` together.

## User-managed reverse proxy

Raw HTTP provides CSRF protection but no transport confidentiality. Do not
enter an administrator password or provider key over an untrusted network.
Terminate HTTPS in a user-managed reverse proxy for Internet-facing use.

For a proxy on the same host, change the web port mapping to:

```yaml
ports:
  - "127.0.0.1:8199:7860"
```

Then proxy to `http://127.0.0.1:8199`. The proxy must:

- preserve the original `Host`, including a non-standard public port;
- leave the browser `Origin` unchanged;
- allow at least 102 MiB request bodies;
- disable request and response buffering for uploads and progress;
- use a read timeout of at least one hour.

For Nginx, the critical host line is:

```nginx
proxy_set_header Host $http_host;
```

`$http_host` is intentional because `$host` can discard the public port. Caddy
preserves the incoming Host by default. A proxy running in another container or
on another machine needs an equivalent private network or firewall rule instead
of the loopback binding.

## Build and verification gates

GitHub Actions performs the release gates:

- Python lint, tests, and package build;
- Rust formatting, Clippy, and all tests;
- React tests and production build;
- Compose/README identity, zero-configuration topology checks, and init
  fail-closed recovery tests;
- amd64 and arm64 builds for `web`, `translator`, and `ops`, merged under the
  exact `sha-<40-character-commit>` tag.

Server acceptance must use those exact images and an isolated Compose project.
At minimum it verifies:

1. direct HTTP access on 8199 and automatic HTTP cookies;
2. preserved-Host HTTPS proxy access and automatic Secure cookies;
3. first-admin claim, logout, and login;
4. anonymous configuration exposing only Google;
5. DeepSeek and disposable OpenAI-compatible configuration without submitting
   a paid-provider job;
6. cancel during translation followed immediately by a successful new job;
7. page release, lease expiry, worker recycle, and coordinator restart;
8. successful mono/dual downloads beginning with `%PDF-`.

All compilation and test commands for a target deployment are run either in
GitHub Actions or on the VPS, never on the end user's machine.

## Upgrade and rollback

Wait for `activeJobs` to reach zero before changing images. Archive the complete
portable directory from its parent so numeric owners and modes are preserved.
Replace `compose.yml`, then run the normal `up` command. An upgrade that removes
older services should include `--remove-orphans` only after the backup exists.

Rollback restores the whole archived portable unit: Compose, SQLite/job data,
and both secret directories. Restoring only SQLite or only the master key is
intentionally unsupported because it can make encrypted provider credentials
undecryptable. Old certificate material from a former gateway-based deployment
is not part of the new runtime and should be retained separately only for the
rollback window.
