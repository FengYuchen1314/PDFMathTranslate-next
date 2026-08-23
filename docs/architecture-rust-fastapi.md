# Rust + FastAPI Web Architecture

The public web application no longer runs on Gradio. It is split into two
application containers, a TLS gateway, an operations image, and one browser
application:

```text
Browser (React + Vite)
       │ HTTPS / SSE
       ▼
Nginx gateway (public IP :7860; port 80 only serves ACME/redirects)
       │ private Compose network
       ▼
Rust web service
  uploads · SQLite state · fair queue · admin sessions · encrypted providers
       │ authenticated private HTTP + shared /data/jobs volume
       ▼
FastAPI translator (private :8000, supervised Uvicorn child)
  Google · DeepSeek V4 · configured OpenAI-compatible · BabelDOC process tree
```

Only the TLS gateway is published. Rust and FastAPI are reachable solely on the
Compose network; FastAPI additionally requires a shared worker token. Automatic
LLM term extraction remains disabled. Provider credentials exist only as
XChaCha20-Poly1305 ciphertext in the Rust-only state volume and as short-lived
in-memory values while Rust starts an authenticated worker job. They are never
part of a browser request, public response, job snapshot, terminal journal, or
shared job file.

The first administrator is created at `/admin`. Production deployments require
the one-time setup token stored outside the application volume. Passwords use
Argon2id. Admin sessions use opaque, hashed server-side tokens and
`HttpOnly; Secure; SameSite=Strict` cookies; writes additionally require the
same-origin request, a CSRF header, and its matching cookie. DeepSeek has a
fixed official base URL and only `deepseek-v4-flash` and
`deepseek-v4-pro`. Administrators can add multiple HTTPS OpenAI-compatible
providers, each with an explicit model allow-list and one default model.

## Cancellation invariant

Rust owns the public job state and the single translation permit. A job moves
through `queued`, `running`, `cancelling`, and one of the three terminal states:
`succeeded`, `failed`, or `cancelled`.

The permit is not released when a browser merely requests cancellation. It is
held until FastAPI reports a terminal state after the Python async generator has
closed, which in turn waits for the BabelDOC subprocess group, pipes, queues,
and listener threads to be reclaimed. Ambiguous start responses and broken
progress streams enter reconciliation: Rust repeatedly sends an idempotent
internal cancellation until the worker confirms a terminal state.

Both services record durable tombstones for canonical job UUIDs even when a
start request has not arrived yet. Rust also rechecks the tombstone after
installing the pending-upload reservation. This closes both sides of the race
where a timed-out upload could begin after its matching cancel or release
request. Terminal-job tombstones remain for six hours. Unknown pre-start IDs
use a separate 15-minute table capped at 4096 rows, preventing anonymous beacon
traffic from growing SQLite without bound. Rust persists snapshots in SQLite.
On restart it reconciles every formerly active job with FastAPI before accepting
new work.
SQLite lives in the web-only `/data/state` volume; the shared `/data/jobs`
volume contains only per-job input and output files.
Before Rust opens its public listener it places FastAPI in drain mode, cancels
every worker-side active job (including IDs missing from SQLite), verifies the
worker is idle, and only then resumes new starts. UUIDs recovered from corrupt
SQLite rows receive an explicit cancellation tombstone before the row is
discarded. A lost or damaged state volume therefore cannot admit a new job
beside an orphaned translation.

Cancellation also has a fail-stop boundary. If Python cleanup misses its hard
deadline, the Uvicorn child terminates. A separate supervisor then exits the
container non-zero instead of starting another child in the old container.
Docker destroys the complete old cgroup (including BabelDOC processes that made
their own process group) and restarts a clean container. Rust keeps its permit
during that restart, then reconciles the unknown job through the cancellation
tombstone before admitting the next task. The supervisor uses the same fail-stop
path when the authenticated health endpoint remains unresponsive.

The browser sends a heartbeat every 10 seconds. Leaving the page starts a short
release grace instead of cancelling immediately: a reload can restore and renew
the lease, while a tab that really went away is reclaimed after 15 seconds.
The ordinary 180-second lease remains the fallback for crashes where no beacon
was delivered and is long enough to tolerate background-tab timer throttling.
Releasing a terminal job only starts the grace window, and a later heartbeat
can renew it. The explicit `DELETE` endpoint disposes a terminal job immediately;
successful files otherwise remain available for the six-hour retention window.

## Public API

- `GET /api/health`
- `GET /api/config`
- `POST /api/jobs` (multipart PDF plus configured provider/model IDs;
  idempotent client UUID in `X-Job-Id`)
- `GET /api/jobs/{id}`
- `GET /api/jobs/{id}/events` (SSE)
- `POST /api/jobs/{id}/cancel`
- `POST /api/jobs/{id}/heartbeat`
- `POST /api/jobs/{id}/release`
- `DELETE /api/jobs/{id}` (explicit terminal cleanup)
- `GET /api/jobs/{id}/files/{mono|dual|glossary}`

Admin control-plane API (all responses are `Cache-Control: no-store`):

- `GET /api/admin/session`
- `POST /api/admin/setup`
- `POST /api/admin/login`
- `POST /api/admin/logout`
- `GET /api/admin/providers`
- `PUT /api/admin/providers/deepseek`
- `POST /api/admin/providers`
- `PUT /api/admin/providers/{id}`
- `DELETE /api/admin/providers/{id}?expectedRevision=N`

Job requests submit only `provider_id`, `model_id`, `thinking_mode`, and
`reasoning_effort`. Anonymous sessions can select only Google; a credentialed
provider is returned to and accepted from an authenticated administrator only.
Provider base URLs and keys are resolved from an accepted, revisioned
server-side profile. Updating or deleting a profile does not mutate an already
admitted job. Generic base URLs must use HTTPS and cannot contain userinfo, a
query, a fragment, local hostnames, or private/reserved addresses; the worker
resolves and checks the destination again immediately before use and does not
honor environment proxies or redirects.

Uploads are streamed to a per-job UUID directory, checked for the PDF magic
header, bounded by `PDF2ZH_MAX_UPLOAD_BYTES`, and covered by a finite upload
deadline. A global admission limit also bounds pending uploads; the translation
permit itself remains exactly one. Downloads use deterministic artifact names
and never accept a filesystem path from the client. If the browser loses the
POST response, it polls the reserved UUID and retries only that same id until
the server confirms either the job or its tombstone.

FastAPI publishes a terminal event only after output PDFs and a compact terminal
journal have been flushed and atomically renamed. A replacement worker epoch can
therefore answer reconciliation without replaying an unbounded event history.
Corrupt journals are quarantined and replaced with a durable failed tombstone.
All drain and recycle waits are bounded; failure to prove a clean new worker
epoch exits the coordinator non-zero so Docker can replace its cgroup.

## Portable Compose deployment

New installations use the repository-root `compose.yml`. The server downloads
prebuilt GHCR images and does not need the source tree, Dockerfiles, or helper
scripts. This deployment contract requires rootful Docker on Linux, Compose
v2.24 or newer, and a local POSIX filesystem; rootless Docker, Windows bind
mount permissions, and remote Docker contexts are not covered. On
`185.99.135.224`, place that one file in an empty directory and run:

```bash
docker compose up -d --pull always --wait --wait-timeout 600
```

The one-shot `init` service creates and validates all credentials, storage, and
the temporary bootstrap certificate. The gateway then serves the ACME webroot;
the long-running certbot service requests the trusted short-lived IP
certificate, copies a validated key/certificate pair into `tls/active`, and
Nginx reloads it. A bounded exponential retry avoids certificate-authority
rate-limit storms. `docker compose --wait` returns successfully only after the
trusted certificate is installed.

All durable material is relative to the Compose file:

- `data/jobs`: uploaded and translated job artifacts.
- `data/state`: Rust SQLite state, sessions, and encrypted provider records.
- `data/certbot`: renewal health state.
- `secrets/web`: provider master key, first-admin token, and web worker token.
- `secrets/worker`: the translator-only copy of the worker token.
- `tls`: ACME, certificate lineage, and active gateway certificate.

Copy the entire directory while the stack is stopped to migrate it. Never copy
`data/state` without `secrets/web`, because encrypted provider credentials are
deliberately unrecoverable without their original master key. The initializer
fails closed instead of silently replacing a lost key. For a different IPv4
address or public port, edit the top-level `x-public-ip` and `x-public-port`
values in the Compose file so the settings survive every later invocation. A
complete release can be selected with
`PDF2ZH_IMAGE_TAG=sha-<full-40-character-commit>`; the distributed file already
defaults to one immutable, tested commit tag.

## Legacy VPS release engineering (historical)

The remainder of this document records the earlier source-on-VPS release and
rollback process. It remains useful for forensic recovery of the existing host,
but it is not the installation path for new users.

Everything in this section is run on `185.99.135.224`. Do not format source,
generate a lock file, run a test, compile, or build an image on a developer
machine and then treat that result as a release artifact. Stage each candidate
in its own directory, for example
`/docker/pdf2zh-rust/releases/20260823-admin-provider-v1`, and never edit that
directory after the final images have been built.

The examples below assume a root login shell (`sudo -i` once, before the
commands), not a mixture of root-owned and unprivileged files:

```bash
set -Eeuo pipefail
umask 077

export PDF2ZH_SERVICE_ROOT=/docker/pdf2zh-rust
export PDF2ZH_RELEASE_ID=20260823-admin-provider-v1
export PDF2ZH_RELEASE_DIR="${PDF2ZH_SERVICE_ROOT}/releases/${PDF2ZH_RELEASE_ID}"
export PDF2ZH_PROD_PROJECT=pdf2zh-next
export PDF2ZH_PROD_ENV="${PDF2ZH_SERVICE_ROOT}/app/.env"
export PDF2ZH_LONG_TEST_PDF=/root/pdf2zh-testdata/long.pdf
export PDF2ZH_SHORT_TEST_PDF=/root/pdf2zh-testdata/short.pdf
export PDF2ZH_LEASE_TEST_PDF=/root/pdf2zh-testdata/lease-stress.pdf

test -d "${PDF2ZH_RELEASE_DIR}/.git" -o -f "${PDF2ZH_RELEASE_DIR}/pyproject.toml"
test -s "${PDF2ZH_LONG_TEST_PDF}" \
  -a -s "${PDF2ZH_SHORT_TEST_PDF}" \
  -a -s "${PDF2ZH_LEASE_TEST_PDF}"
case "${PDF2ZH_RELEASE_ID}" in
  (*[!a-zA-Z0-9_.-]*|'') echo 'unsafe release id' >&2; exit 1 ;;
esac
(( ${#PDF2ZH_RELEASE_ID} <= 64 )) || {
  echo 'release id is too long' >&2
  exit 1
}
```

### VPS-only validation and immutable images

Use disposable tool containers on the VPS. The temporary Python test image is
not a release image; it supplies the complete worker dependency graph while the
bind-mounted candidate source is formatted and tested:

```bash
cd "${PDF2ZH_RELEASE_DIR}"

docker build --pull \
  -f deploy/Dockerfile.worker \
  -t "pdf2zh-next-translator:test-${PDF2ZH_RELEASE_ID}" .
docker run --rm --user 0:0 \
  -v "${PDF2ZH_RELEASE_DIR}:/src" -w /src \
  "pdf2zh-next-translator:test-${PDF2ZH_RELEASE_ID}" \
  sh -ec '
    uv pip install --system --no-cache pytest ruff
    python -m ruff format \
      pdf2zh_next/worker_api.py \
      pdf2zh_next/translator/translator_impl/openai.py \
      pdf2zh_next/config/translate_engine_model.py \
      tests/test_worker_api.py tests/test_deepseek_thinking_mode.py
    python -m ruff check \
      pdf2zh_next/worker_api.py \
      pdf2zh_next/translator/translator_impl/openai.py \
      pdf2zh_next/config/translate_engine_model.py \
      tests/test_worker_api.py tests/test_deepseek_thinking_mode.py
    python -m pytest -q
  '
docker image rm "pdf2zh-next-translator:test-${PDF2ZH_RELEASE_ID}"
```

`Cargo.lock` is generated or updated on the VPS only when dependencies were
intentionally changed. Do that once, review and retain the resulting lock file
in the release/source history, then use the routine locked path for this and
every later build. Do **not** run `cargo generate-lockfile` on every deployment:

```bash
# One-time dependency-change step only (omit for a routine release):
docker run --rm --user 0:0 \
  -v "${PDF2ZH_RELEASE_DIR}:/src" -w /src/backend-rs \
  -v pdf2zh-cargo-registry:/usr/local/cargo/registry \
  rust:1.96-slim-bookworm \
  sh -ec 'cargo generate-lockfile'

# Routine validation: every dependency-resolving Cargo command is locked.
docker run --rm --user 0:0 \
  -v "${PDF2ZH_RELEASE_DIR}:/src" -w /src/backend-rs \
  -v pdf2zh-cargo-registry:/usr/local/cargo/registry \
  -v "pdf2zh-rust-target-${PDF2ZH_RELEASE_ID}:/src/backend-rs/target" \
  rust:1.96-slim-bookworm \
  sh -ec '
    cargo fmt --all
    cargo fmt --all -- --check
    cargo fetch --locked
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked --all-targets
  '

docker run --rm --user 0:0 \
  -v "${PDF2ZH_RELEASE_DIR}/frontend:/src" -w /src \
  -v "pdf2zh-node-modules-${PDF2ZH_RELEASE_ID}:/src/node_modules" \
  -v pdf2zh-npm-cache:/root/.npm \
  node:24-bookworm-slim \
  sh -ec 'npm ci && npm test && npm run build'
```

Run a final source-diff review on the VPS after the formatters. If the lock file
or formatted source changed, retain those exact VPS-generated files before
building. Release tags are single-use: refuse to build if any of the three tags
already exists. A separate build env avoids changing production before
acceptance:

```bash
build_env="${PDF2ZH_RELEASE_DIR}/.build.env"
build_tmp="$(mktemp "${build_env}.tmp.XXXXXX")"
{
  printf 'PDF2ZH_WORKER_TOKEN=%s\n' "$(openssl rand -hex 32)"
  printf 'PDF2ZH_APP_IMAGE_TAG=%s\n' "${PDF2ZH_RELEASE_ID}"
  printf 'PDF2ZH_GATEWAY_IMAGE_TAG=%s\n' "${PDF2ZH_RELEASE_ID}"
} >"${build_tmp}"
chmod 0600 "${build_tmp}"
mv -f -- "${build_tmp}" "${build_env}"

for image in \
  "pdf2zh-next-web:${PDF2ZH_RELEASE_ID}" \
  "pdf2zh-next-translator:${PDF2ZH_RELEASE_ID}" \
  "pdf2zh-next-gateway:${PDF2ZH_RELEASE_ID}"; do
  if docker image inspect "${image}" >/dev/null 2>&1; then
    echo "refusing to overwrite release image ${image}" >&2
    exit 1
  fi
done

build_project="pdf2zh-build-${PDF2ZH_RELEASE_ID,,}"
build_project="${build_project//./-}"
docker compose -p "${build_project}" --profile tls \
  --env-file "${build_env}" -f deploy/compose.vps.yml config --quiet
bash script/vps_deploy_config_regression.sh
bash script/vps_certbot_renew_loop_regression.sh
docker compose -p "${build_project}" --profile tls \
  --env-file "${build_env}" -f deploy/compose.vps.yml \
  build --pull translator web gateway

image_manifest="${PDF2ZH_RELEASE_DIR}/release-images.txt"
manifest_tmp="$(mktemp "${image_manifest}.tmp.XXXXXX")"
for image in \
  "pdf2zh-next-web:${PDF2ZH_RELEASE_ID}" \
  "pdf2zh-next-translator:${PDF2ZH_RELEASE_ID}" \
  "pdf2zh-next-gateway:${PDF2ZH_RELEASE_ID}"; do
  printf '%s %s\n' "${image}" \
    "$(docker image inspect --format '{{.Id}}' "${image}")"
done >"${manifest_tmp}"
chmod 0600 "${manifest_tmp}"
mv -f -- "${manifest_tmp}" "${image_manifest}"
```

Prepare the host-only encryption key, setup token, and trusted short-lived IP
certificate. Port 80 must remain publicly reachable for HTTP-01 renewal. Use
the initialization flag only for the first creation of this installation's
provider key. That includes a verified migration from the pre-admin release,
which never had encrypted provider state. Once an admin-capable release has
used the key, every upgrade must omit the flag so key loss fails closed:

```bash
cd "${PDF2ZH_RELEASE_DIR}"

# Confirmed first install or verified pre-admin migration only:
# PDF2ZH_INITIALIZE_MASTER_KEY=true \
#   bash deploy/bootstrap-vps-secrets-and-tls.sh

# Existing installation / ordinary upgrade:
bash deploy/bootstrap-vps-secrets-and-tls.sh
```

### Isolated loopback acceptance

Acceptance uses the final tagged application and gateway images, a unique
Compose project, a unique directory, loopback-only HTTP, disposable secrets,
and disposable named volumes. Run the whole block in the same root shell. Its
trap always tears down the **exact** acceptance project and deletes the exact
env/secret files. The base Compose file must precede the overlay.

```bash
set -Eeuo pipefail
umask 077

acceptance_id="${PDF2ZH_RELEASE_ID,,}-$(date -u +%Y%m%d%H%M%S)-$(openssl rand -hex 4)"
acceptance_id="${acceptance_id//./-}"
acceptance_project="pdf2zh-acceptance-${acceptance_id}"
acceptance_root="${PDF2ZH_SERVICE_ROOT}/acceptance/${acceptance_id}"
acceptance_env="${acceptance_root}/acceptance.env"
acceptance_master="${acceptance_root}/provider-master-key"
acceptance_setup="${acceptance_root}/admin-setup-token"
acceptance_port=17860

case "${acceptance_root}" in
  ("${PDF2ZH_SERVICE_ROOT}"/acceptance/*) ;;
  (*) echo 'unsafe acceptance directory' >&2; exit 1 ;;
esac
acceptance_compose=(
  docker compose -p "${acceptance_project}"
  --env-file "${acceptance_env}"
  -f "${PDF2ZH_RELEASE_DIR}/deploy/compose.vps.yml"
  -f "${PDF2ZH_RELEASE_DIR}/deploy/compose.acceptance.yml"
)
acceptance_cleanup() {
  "${acceptance_compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
  rm -f -- "${acceptance_env}" "${acceptance_master}" "${acceptance_setup}"
  rmdir -- "${acceptance_root}" 2>/dev/null || true
}
trap acceptance_cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

install -d -m 0700 "${acceptance_root}"
openssl rand -base64 32 >"${acceptance_master}"
openssl rand -base64 24 | tr -d '\n' >"${acceptance_setup}"
chown 10001:10001 "${acceptance_master}" "${acceptance_setup}"
chmod 0600 "${acceptance_master}" "${acceptance_setup}"

acceptance_tmp="$(mktemp "${acceptance_env}.tmp.XXXXXX")"
{
  printf 'PDF2ZH_WORKER_TOKEN=%s\n' "$(openssl rand -hex 32)"
  printf 'PDF2ZH_APP_IMAGE_TAG=%s\n' "${PDF2ZH_RELEASE_ID}"
  printf 'PDF2ZH_GATEWAY_IMAGE_TAG=%s\n' "${PDF2ZH_RELEASE_ID}"
  printf 'PDF2ZH_ACCEPTANCE_PORT=%s\n' "${acceptance_port}"
  printf 'PDF2ZH_ACCEPTANCE_MASTER_KEY_FILE_HOST=%s\n' "${acceptance_master}"
  printf 'PDF2ZH_ACCEPTANCE_SETUP_TOKEN_FILE_HOST=%s\n' "${acceptance_setup}"
  printf 'PDF2ZH_LEASE_SECONDS=15\n'
  printf 'PDF2ZH_RELEASE_GRACE_SECONDS=5\n'
  printf 'PDF2ZH_CANCEL_RECONCILE_SECONDS=60\n'
  printf 'PDF2ZH_WORKER_RECYCLE_SECONDS=60\n'
} >"${acceptance_tmp}"
chmod 0600 "${acceptance_tmp}"
mv -f -- "${acceptance_tmp}" "${acceptance_env}"

"${acceptance_compose[@]}" config --quiet
"${acceptance_compose[@]}" up -d --no-build --wait --wait-timeout 600

acceptance_scan_args=()
for acceptance_volume_label in web-state jobs; do
  mapfile -t acceptance_volumes < <(
    docker volume ls --format '{{.Name}}' \
      --filter "label=com.docker.compose.project=${acceptance_project}" \
      --filter "label=com.docker.compose.volume=${acceptance_volume_label}"
  )
  if (( ${#acceptance_volumes[@]} != 1 )); then
    printf 'expected exactly one disposable %s volume\n' \
      "${acceptance_volume_label}" >&2
    exit 1
  fi
  acceptance_mount="$(docker volume inspect --format '{{.Mountpoint}}' \
    "${acceptance_volumes[0]}")"
  case "${acceptance_mount}" in
    (/*) ;;
    (*) echo 'acceptance volume mountpoint is empty or not absolute' >&2; exit 1 ;;
  esac
  test -n "${acceptance_mount}"
  test "${acceptance_mount}" != /
  test -d "${acceptance_mount}"
  acceptance_scan_args+=(--scan-root "${acceptance_mount}")
done

python3 "${PDF2ZH_RELEASE_DIR}/script/vps_admin_provider_regression.py" \
  "http://127.0.0.1:${acceptance_port}" \
  --setup-token-file "${acceptance_setup}" \
  "${acceptance_scan_args[@]}" \
  --compose-project "${acceptance_project}"
python3 "${PDF2ZH_RELEASE_DIR}/script/vps_rust_api_regression.py" \
  "http://127.0.0.1:${acceptance_port}" \
  "${PDF2ZH_LONG_TEST_PDF}" "${PDF2ZH_SHORT_TEST_PDF}" \
  --rounds 3 --test-lease
python3 "${PDF2ZH_RELEASE_DIR}/script/vps_worker_failstop_regression.py" \
  "http://127.0.0.1:${acceptance_port}" "${PDF2ZH_LONG_TEST_PDF}" \
  --compose-project "${acceptance_project}"

# Validate the exact release gateway image and its production bind mounts.
# `run` does not publish the gateway ports; it joins the acceptance network so
# the `web` upstream in nginx.vps.conf resolves during `nginx -t`.
"${acceptance_compose[@]}" --profile tls run --rm --no-deps gateway nginx -t
"${acceptance_compose[@]}" --profile tls run --rm --no-deps \
  --entrypoint /bin/sh gateway -ec '
    cert=/etc/letsencrypt/live/185.99.135.224/fullchain.pem
    key=/etc/letsencrypt/live/185.99.135.224/privkey.pem
    test -r "$cert" -a -r "$key"
    openssl x509 -checkend 86400 -noout -in "$cert"
    openssl x509 -noout -ext subjectAltName -in "$cert" \
      | grep -F "IP Address:185.99.135.224"
    cert_pub="$(openssl x509 -pubkey -noout -in "$cert" \
      | openssl pkey -pubin -outform DER | openssl sha256)"
    key_pub="$(openssl pkey -pubout -outform DER -in "$key" \
      | openssl sha256)"
    test -n "$cert_pub" -a "$cert_pub" = "$key_pub"
  '

"${acceptance_compose[@]}" down -v --remove-orphans
rm -f -- "${acceptance_env}" "${acceptance_master}" "${acceptance_setup}"
rmdir -- "${acceptance_root}"
trap - EXIT INT TERM

test -z "$(docker ps -aq \
  --filter "label=com.docker.compose.project=${acceptance_project}")"
test -z "$(docker volume ls -q \
  --filter "label=com.docker.compose.project=${acceptance_project}")"
```

Never reuse a fixed acceptance project name. Before proceeding, verify that no
container or volume with the generated project label remains. A failed residue
check is a release blocker, not a reason to broaden the cleanup target.

### Consistent production backup and cutover

Do not touch production until acceptance has passed. For an upgrade, first
confirm `activeJobs` is zero. If it is not zero, wait for completion or cancel
through the normal API and wait for the terminal state; do not force the
cutover. Stop ingress and Certbot next, recheck through the private web
container so no request won the race, then stop web and translator cleanly.
Stopping Certbot also freezes the certificate files while they are archived.

The following backup is intentionally created with mode `0700` and `umask 077`.
It includes the old env, both host secrets, state/jobs, Certbot state when
present, the certificate tree, the old deployment directory (including the
bind-mounted Nginx configuration and reload script), the old images, and the
legacy orphan cache/config volumes when they exist.

```bash
set -Eeuo pipefail
umask 077

service_container() {
  local service="$1"
  local -a found=()
  mapfile -t found < <(
    docker ps -aq \
      --filter "label=com.docker.compose.project=${PDF2ZH_PROD_PROJECT}" \
      --filter "label=com.docker.compose.service=${service}"
  )
  (( ${#found[@]} == 1 )) || {
    echo "expected exactly one production ${service} container" >&2
    return 1
  }
  printf '%s\n' "${found[0]}"
}

optional_service_container() {
  local service="$1"
  local -a found=()
  mapfile -t found < <(
    docker ps -aq \
      --filter "label=com.docker.compose.project=${PDF2ZH_PROD_PROJECT}" \
      --filter "label=com.docker.compose.service=${service}"
  )
  (( ${#found[@]} <= 1 )) || {
    echo "found multiple production ${service} containers" >&2
    return 1
  }
  if (( ${#found[@]} == 1 )); then
    printf '%s\n' "${found[0]}"
  fi
}

old_web="$(service_container web)"
old_translator="$(service_container translator)"
old_gateway="$(optional_service_container gateway)"
old_certbot="$(optional_service_container certbot)"

assert_old_web_idle() {
  docker exec "${old_web}" \
    curl --fail --silent http://127.0.0.1:7860/api/health \
    | python3 -c 'import json,sys; p=json.load(sys.stdin); assert p["activeJobs"] == 0, p'
}

assert_old_web_idle
if [[ -n "${old_certbot}" ]]; then
  docker stop --time 30 "${old_certbot}"
fi
if [[ -n "${old_gateway}" ]]; then
  docker stop --time 30 "${old_gateway}"
  assert_old_web_idle
fi
docker stop --time 360 "${old_web}"
docker stop --time 60 "${old_translator}"

backup_id="$(date -u +%Y%m%dT%H%M%SZ)-pre-${PDF2ZH_RELEASE_ID}"
backup_root="${PDF2ZH_SERVICE_ROOT}/backups/${backup_id}"
case "${backup_root}" in
  ("${PDF2ZH_SERVICE_ROOT}"/backups/*) ;;
  (*) echo 'unsafe backup path' >&2; exit 1 ;;
esac
install -d -m 0700 "${backup_root}" "${backup_root}/volumes"

old_compose_anchor="${old_gateway:-${old_web}}"
old_workdir="$(docker inspect --format \
  '{{ index .Config.Labels "com.docker.compose.project.working_dir" }}' \
  "${old_compose_anchor}")"
case "${old_workdir}" in
  (/*) ;;
  (*) echo 'old Compose working directory is not absolute' >&2; exit 1 ;;
esac
if [[ -d "${old_workdir}/deploy" ]]; then
  old_source_root="${old_workdir}"
elif [[ "${old_workdir%/}" == */deploy ]] \
    && [[ -f "${old_workdir%/}/compose.vps.yml" ]]; then
  # Compose records either the repository root or the directory containing
  # the first compose file, depending on how the legacy command was invoked.
  old_source_root="${old_workdir%/deploy}"
else
  echo 'cannot resolve the old deployment source root' >&2
  exit 1
fi
case "${old_source_root}" in
  (/*) ;;
  (*) echo 'old deployment source root is not absolute' >&2; exit 1 ;;
esac
test -d "${old_source_root}/deploy"

install -m 0600 "${PDF2ZH_PROD_ENV}" "${backup_root}/production.env"
install -m 0600 "${PDF2ZH_SERVICE_ROOT}/secrets/provider-master-key" \
  "${backup_root}/provider-master-key"
install -m 0600 "${PDF2ZH_SERVICE_ROOT}/secrets/admin-setup-token" \
  "${backup_root}/admin-setup-token"
install -m 0600 \
  "${PDF2ZH_RELEASE_DIR}/deploy/compose.rollback-images.yml" \
  "${backup_root}/compose.rollback-images.yml"
install -m 0600 \
  "${PDF2ZH_RELEASE_DIR}/deploy/compose.rollback-legacy-tls.yml" \
  "${backup_root}/compose.rollback-legacy-tls.yml"
tar -C "${old_source_root}" -czf \
  "${backup_root}/deployment-config.tar.gz" deploy
tar -C "${PDF2ZH_SERVICE_ROOT}" -czf "${backup_root}/tls.tar.gz" tls

mount_volume() {
  local container="$1" destination="$2"
  docker inspect "${container}" \
    | python3 -c '
import json, sys
destination = sys.argv[1]
matches = [m for m in json.load(sys.stdin)[0]["Mounts"]
           if m["Destination"] == destination and m["Type"] == "volume"]
assert len(matches) == 1, matches
print(matches[0]["Name"])
' "${destination}"
}

web_state_volume="$(mount_volume "${old_web}" /data/state)"
jobs_volume="$(mount_volume "${old_web}" /data/jobs)"
printf '%s=%s\n' \
  web-state "${web_state_volume}" \
  jobs "${jobs_volume}" \
  >"${backup_root}/volume-map"
certbot_state_volume=""
if [[ -n "${old_certbot}" ]]; then
  certbot_state_volume="$(mount_volume "${old_certbot}" /var/lib/letsencrypt)"
  printf '%s=%s\n' certbot-state "${certbot_state_volume}" \
    >>"${backup_root}/volume-map"
fi

snapshot_volume() {
  local volume="$1" snapshot="$2"
  test -n "${volume}" -a -n "${snapshot}"
  docker volume inspect "${volume}" >/dev/null
  docker run --rm \
    -e SNAPSHOT="${snapshot}" \
    -v "${volume}:/source:ro" \
    -v "${backup_root}/volumes:/backup" \
    busybox:1.37.0@sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0 \
    sh -ec '
      cd /source
      tar -czf "/backup/${SNAPSHOT}.tar.gz" .
      find . -type f -exec sha256sum {} \; | sort \
        >"/backup/${SNAPSHOT}.sha256"
    '
}

snapshot_volume "${web_state_volume}" web-state
snapshot_volume "${jobs_volume}" jobs
if [[ -n "${certbot_state_volume}" ]]; then
  snapshot_volume "${certbot_state_volume}" certbot-state
fi

for legacy in translator-cache translator-config; do
  mapfile -t legacy_volumes < <(
    docker volume ls --format '{{.Name}}' \
      --filter "label=com.docker.compose.project=${PDF2ZH_PROD_PROJECT}" \
      --filter "label=com.docker.compose.volume=${legacy}"
  )
  if (( ${#legacy_volumes[@]} > 1 )); then
    echo "ambiguous legacy ${legacy} volumes" >&2
    exit 1
  elif (( ${#legacy_volumes[@]} == 1 )); then
    snapshot_volume "${legacy_volumes[0]}" "legacy-${legacy}"
    printf '%s=%s\n' "legacy-${legacy}" "${legacy_volumes[0]}" \
      >>"${backup_root}/volume-map"
  fi
done

rollback_tag="rollback-${backup_id,,}"
rollback_tag="${rollback_tag//:/-}"
if [[ -n "${old_gateway}" ]]; then
  rollback_gateway_tag="${rollback_tag}"
else
  # A legacy deployment has no gateway image to save. Its TLS rollback keeps
  # using the already-accepted candidate gateway around the old app images.
  rollback_gateway_tag="${PDF2ZH_RELEASE_ID}"
fi
rollback_pairs=( \
  "${old_web}:pdf2zh-next-web" \
  "${old_translator}:pdf2zh-next-translator" \
)
rollback_images=( \
  "pdf2zh-next-web:${rollback_tag}" \
  "pdf2zh-next-translator:${rollback_tag}" \
)
if [[ -n "${old_gateway}" ]]; then
  rollback_pairs+=("${old_gateway}:pdf2zh-next-gateway")
  rollback_images+=("pdf2zh-next-gateway:${rollback_tag}")
else
  # Make the pre-TLS rollback archive self-contained instead of assuming the
  # accepted gateway tag will still be present in the local Docker daemon.
  rollback_images+=("pdf2zh-next-gateway:${PDF2ZH_RELEASE_ID}")
fi
for pair in "${rollback_pairs[@]}"; do
  container="${pair%%:*}"
  repository="${pair#*:}"
  image_id="$(docker inspect --format '{{.Image}}' "${container}")"
  docker image tag "${image_id}" "${repository}:${rollback_tag}"
done
docker image save -o "${backup_root}/rollback-images.tar" \
  "${rollback_images[@]}"
printf '%s\n' "${rollback_tag}" >"${backup_root}/rollback-tag"
printf '%s\n' "${rollback_gateway_tag}" \
  >"${backup_root}/rollback-gateway-tag"
{
  for image in \
    "pdf2zh-next-web:${rollback_tag}" \
    "pdf2zh-next-translator:${rollback_tag}" \
    "pdf2zh-next-gateway:${rollback_gateway_tag}"; do
    printf '%s %s\n' "${image}" \
      "$(docker image inspect --format '{{.Id}}' "${image}")"
  done
} >"${backup_root}/rollback-image-ids"
```

Create `rollback.env` from the backed-up env by collapsing duplicate keys. Its
app tag is the saved rollback tag; its gateway tag is also the rollback tag when
the old release had a gateway, otherwise it is the already-accepted candidate
release tag used by the legacy TLS overlay. It deliberately preserves legacy
settings, including `PDF2ZH_DEEPSEEK_API_KEY`, because the old images may still
require them. Candidate mode removes that obsolete key, rotates the internal
worker token, and forcibly sets the public origin/cookie policy to HTTPS. The
rollback token is preserved for the old image pair, and the final file is
always `0600`:

```bash
atomic_release_env() {
  local source="$1" destination="$2" app_tag="$3" gateway_tag="$4" mode="$5"
  python3 - "${source}" "${destination}" "${app_tag}" "${gateway_tag}" "${mode}" <<'PY'
import os
import pathlib
import re
import secrets
import sys

source, destination = map(pathlib.Path, sys.argv[1:3])
app_tag, gateway_tag = sys.argv[3:5]
mode = sys.argv[5]
if mode not in {"rollback", "candidate"}:
    raise SystemExit(f"unsupported release-env mode: {mode!r}")
pattern = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)=(.*)")
values = {}
order = []
comments = []
if source.exists():
    for raw in source.read_text(encoding="utf-8").splitlines():
        match = pattern.fullmatch(raw)
        if match:
            key, value = match.groups()
            if key not in values:
                order.append(key)
            values[key] = value  # Last value wins; output still contains it once.
        elif not raw.strip() or raw.lstrip().startswith("#"):
            comments.append(raw)
        else:
            raise SystemExit(f"unsupported env line; refusing rewrite: {raw!r}")

if mode == "candidate":
    values["PDF2ZH_WORKER_TOKEN"] = secrets.token_hex(32)
else:
    values["PDF2ZH_WORKER_TOKEN"] = (
        values.get("PDF2ZH_WORKER_TOKEN") or secrets.token_hex(32)
    )
values["PDF2ZH_APP_IMAGE_TAG"] = app_tag
values["PDF2ZH_GATEWAY_IMAGE_TAG"] = gateway_tag
required_keys = ["PDF2ZH_WORKER_TOKEN", "PDF2ZH_APP_IMAGE_TAG",
                 "PDF2ZH_GATEWAY_IMAGE_TAG"]
if mode == "candidate":
    values.pop("PDF2ZH_DEEPSEEK_API_KEY", None)
    values["PDF2ZH_PUBLIC_ORIGIN"] = "https://185.99.135.224:7860"
    values["PDF2ZH_SECURE_COOKIES"] = "true"
    required_keys.extend(["PDF2ZH_PUBLIC_ORIGIN", "PDF2ZH_SECURE_COOKIES"])
for key in required_keys:
    if key not in order:
        order.append(key)
if mode == "candidate":
    order = [key for key in order if key != "PDF2ZH_DEEPSEEK_API_KEY"]

destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
temporary = destination.with_name(destination.name + f".tmp.{os.getpid()}")
flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
fd = os.open(temporary, flags, 0o600)
try:
    with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
        if comments:
            handle.write("\n".join(comments).rstrip() + "\n")
        for key in order:
            handle.write(f"{key}={values[key]}\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.chmod(temporary, 0o600)
    os.replace(temporary, destination)
    directory_fd = os.open(destination.parent, os.O_RDONLY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
finally:
    if temporary.exists():
        temporary.unlink()
PY
}

atomic_release_env \
  "${backup_root}/production.env" "${backup_root}/rollback.env" \
  "${rollback_tag}" "${rollback_gateway_tag}" rollback
atomic_release_env \
  "${backup_root}/production.env" "${PDF2ZH_PROD_ENV}" \
  "${PDF2ZH_RELEASE_ID}" "${PDF2ZH_RELEASE_ID}" candidate
```

Before starting the candidate, perform a restore drill into uniquely named
disposable volumes. This checks archive readability and every backed-up file
hash without modifying production volumes:

```bash
drill_id="pdf2zh-restore-drill-$(openssl rand -hex 6)"
drill_volumes=()
drill_cleanup() {
  if (( ${#drill_volumes[@]} )); then
    docker volume rm "${drill_volumes[@]}" >/dev/null 2>&1 || true
  fi
}
trap drill_cleanup EXIT INT TERM

drill_snapshots=(web-state jobs)
if [[ -s "${backup_root}/volumes/certbot-state.tar.gz" ]]; then
  drill_snapshots+=(certbot-state)
fi
for snapshot in "${drill_snapshots[@]}"; do
  drill_volume="${drill_id}-${snapshot}"
  docker volume create "${drill_volume}" >/dev/null
  drill_volumes+=("${drill_volume}")
  docker run --rm \
    -e SNAPSHOT="${snapshot}" \
    -v "${drill_volume}:/restore" \
    -v "${backup_root}/volumes:/backup:ro" \
    busybox:1.37.0@sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0 \
    sh -ec '
      tar -xzf "/backup/${SNAPSHOT}.tar.gz" -C /restore
      cd /restore
      if test -s "/backup/${SNAPSHOT}.sha256"; then
        sha256sum -c "/backup/${SNAPSHOT}.sha256"
      else
        test -z "$(find . -type f -print -quit)"
      fi
    '
done
tar -tzf "${backup_root}/deployment-config.tar.gz" >/dev/null
tar -tzf "${backup_root}/tls.tar.gz" >/dev/null
tar -tf "${backup_root}/rollback-images.tar" >/dev/null
drill_cleanup
drill_volumes=()
trap - EXIT INT TERM
```

Now start production from the accepted release directory. `--no-build`
guarantees that acceptance and production use the same image IDs, and the
600-second wait covers worker warm-up, reconciliation, TLS, and Certbot health:

```bash
cd "${PDF2ZH_RELEASE_DIR}"
production_compose=(
  docker compose -p "${PDF2ZH_PROD_PROJECT}" --profile tls
  --env-file "${PDF2ZH_PROD_ENV}"
  -f "${PDF2ZH_RELEASE_DIR}/deploy/compose.vps.yml"
)
while read -r image expected_id; do
  test -n "${image}" -a -n "${expected_id}"
  actual_id="$(docker image inspect --format '{{.Id}}' "${image}")"
  test "${actual_id}" = "${expected_id}"
done <"${PDF2ZH_RELEASE_DIR}/release-images.txt"
"${production_compose[@]}" config --quiet
"${production_compose[@]}" up -d --no-build --wait --wait-timeout 600
```

On a first installation there are no old containers or volumes to snapshot.
After defining `atomic_release_env`, initialize the env with the complete
five-argument call:

```bash
atomic_release_env \
  /dev/null "${PDF2ZH_PROD_ENV}" \
  "${PDF2ZH_RELEASE_ID}" "${PDF2ZH_RELEASE_ID}" candidate
```

Still create a protected backup of the freshly initialized master/setup keys,
certificate tree, env, and accepted deployment configuration before the first
`up`; thereafter use the full upgrade path above.

There is one absolute prohibition: never run `docker compose ... down -v` (or
manually remove named volumes) for the `pdf2zh-next` production project. `down
-v` appears above only for a randomly named, loopback-only acceptance project.

### Production regression and rollback

Use the trusted certificate, not `curl -k`. Verify health, cancellation/reload
leases, fail-stop recovery, and the first-admin status. The production
regression must not register an administrator or install a dummy paid-provider
key:

```bash
curl --fail --silent --show-error \
  https://185.99.135.224:7860/api/health \
  | python3 -c 'import json,sys; p=json.load(sys.stdin); assert p["activeJobs"] == 0, p'
curl --fail --silent --show-error \
  https://185.99.135.224:7860/api/admin/session \
  | python3 -c 'import json,sys; p=json.load(sys.stdin); assert not p["authenticated"], p'

python3 "${PDF2ZH_RELEASE_DIR}/script/vps_rust_api_regression.py" \
  https://185.99.135.224:7860 \
  "${PDF2ZH_LONG_TEST_PDF}" "${PDF2ZH_SHORT_TEST_PDF}" \
  --rounds 3 --test-lease --lease-pdf "${PDF2ZH_LEASE_TEST_PDF}"
python3 "${PDF2ZH_RELEASE_DIR}/script/vps_worker_failstop_regression.py" \
  https://185.99.135.224:7860 "${PDF2ZH_LONG_TEST_PDF}" \
  --compose-project "${PDF2ZH_PROD_PROJECT}"
"${production_compose[@]}" ps
```

If any production check fails, close ingress and stop the new web/translator
before restoration. Restore the state volume and its matching master key as one
unit; restoring only one makes provider credentials deliberately undecryptable.
Likewise, restore both application tags (web and translator). If the old release
already had TLS, restore its paired gateway tag and archived deployment files.
For a pre-TLS release, keep the accepted candidate gateway/Certbot around the
old application images through the dedicated rollback overlay; this avoids
both a protocol downgrade and the legacy web container's host-port conflict.

```bash
# Resolve and stop only this exact production project's containers. Do not use
# `down`, and especially do not use `down -v`.
new_gateway="$(service_container gateway)"
new_certbot="$(service_container certbot)"
new_web="$(service_container web)"
new_translator="$(service_container translator)"
docker stop --time 30 "${new_certbot}" "${new_gateway}"
docker stop --time 360 "${new_web}"
docker stop --time 60 "${new_translator}"

docker image load -i "${backup_root}/rollback-images.tar"

restore_volume() {
  local volume="$1" snapshot="$2"
  test -n "${volume}" -a -n "${snapshot}"
  docker volume inspect "${volume}" >/dev/null
  docker run --rm \
    -e SNAPSHOT="${snapshot}" \
    -v "${volume}:/restore" \
    -v "${backup_root}/volumes:/backup:ro" \
    busybox:1.37.0@sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0 \
    sh -ec '
      find /restore -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
      tar -xzf "/backup/${SNAPSHOT}.tar.gz" -C /restore
      cd /restore
      if test -s "/backup/${SNAPSHOT}.sha256"; then
        sha256sum -c "/backup/${SNAPSHOT}.sha256"
      else
        test -z "$(find . -type f -print -quit)"
      fi
    '
}

# These names came from the stopped production containers and were written to
# volume-map before the backup. Re-read and compare them before destructive
# restore; abort on an empty, changed, or ambiguous name.
test "$(sed -n 's/^web-state=//p' "${backup_root}/volume-map")" = \
  "${web_state_volume}"
test "$(sed -n 's/^jobs=//p' "${backup_root}/volume-map")" = \
  "${jobs_volume}"
restore_volume "${web_state_volume}" web-state
restore_volume "${jobs_volume}" jobs
if [[ -n "${certbot_state_volume}" ]]; then
  test "$(sed -n 's/^certbot-state=//p' "${backup_root}/volume-map")" = \
    "${certbot_state_volume}"
  restore_volume "${certbot_state_volume}" certbot-state
fi

atomic_install() {
  local source="$1" destination="$2" owner="$3" group="$4"
  local temporary
  temporary="$(mktemp "${destination}.tmp.XXXXXX")"
  install -m 0600 -o "${owner}" -g "${group}" "${source}" "${temporary}"
  mv -f -- "${temporary}" "${destination}"
}
atomic_install "${backup_root}/provider-master-key" \
  "${PDF2ZH_SERVICE_ROOT}/secrets/provider-master-key" 10001 10001
atomic_install "${backup_root}/admin-setup-token" \
  "${PDF2ZH_SERVICE_ROOT}/secrets/admin-setup-token" 10001 10001
atomic_install "${backup_root}/rollback.env" "${PDF2ZH_PROD_ENV}" 0 0

rollback_source="${backup_root}/rollback-source"
install -d -m 0700 "${rollback_source}"
tar -xzf "${backup_root}/deployment-config.tar.gz" -C "${rollback_source}"
rollback_app_tag="$(<"${backup_root}/rollback-tag")"
rollback_gateway_tag="$(<"${backup_root}/rollback-gateway-tag")"
test -n "${rollback_app_tag}" -a -n "${rollback_gateway_tag}"
export PDF2ZH_ROLLBACK_APP_IMAGE_TAG="${rollback_app_tag}"
export PDF2ZH_ROLLBACK_GATEWAY_IMAGE_TAG="${rollback_gateway_tag}"
rollback_force_recreate=false
if [[ -n "${old_gateway}" ]]; then
  # The previous release already had its own TLS topology and gateway image.
  rollback_compose=(
    docker compose -p "${PDF2ZH_PROD_PROJECT}" --profile tls
    --env-file "${PDF2ZH_PROD_ENV}"
    -f "${rollback_source}/deploy/compose.vps.yml"
    -f "${backup_root}/compose.rollback-images.yml"
  )
else
  # A pre-TLS release published web:7860 directly and its worker health body
  # predates the epoch/boot-id contract. Run those backed-up app images behind
  # the accepted gateway/certbot topology. The overlay also restores their
  # cache/config volumes and prevents web from reclaiming the public host port.
  legacy_cache_volume="$(sed -n \
    's/^legacy-translator-cache=//p' "${backup_root}/volume-map")"
  legacy_config_volume="$(sed -n \
    's/^legacy-translator-config=//p' "${backup_root}/volume-map")"
  test -n "${legacy_cache_volume}" -a -n "${legacy_config_volume}"
  test "$(grep -c '^legacy-translator-cache=' "${backup_root}/volume-map")" -eq 1
  test "$(grep -c '^legacy-translator-config=' "${backup_root}/volume-map")" -eq 1
  docker volume inspect "${legacy_cache_volume}" "${legacy_config_volume}" \
    >/dev/null
  export PDF2ZH_LEGACY_TRANSLATOR_CACHE_VOLUME="${legacy_cache_volume}"
  export PDF2ZH_LEGACY_TRANSLATOR_CONFIG_VOLUME="${legacy_config_volume}"
  rollback_compose=(
    docker compose -p "${PDF2ZH_PROD_PROJECT}" --profile tls
    --env-file "${PDF2ZH_PROD_ENV}"
    -f "${PDF2ZH_RELEASE_DIR}/deploy/compose.vps.yml"
    -f "${backup_root}/compose.rollback-legacy-tls.yml"
    -f "${backup_root}/compose.rollback-images.yml"
  )
  rollback_force_recreate=true
fi
"${rollback_compose[@]}" config --quiet
mapfile -t rollback_config_images < <("${rollback_compose[@]}" config --images)
for expected_image in \
  "pdf2zh-next-web:${rollback_app_tag}" \
  "pdf2zh-next-translator:${rollback_app_tag}" \
  "pdf2zh-next-gateway:${rollback_gateway_tag}"; do
  image_count="$(printf '%s\n' "${rollback_config_images[@]}" \
    | grep -Fxc -- "${expected_image}" || true)"
  test "${image_count}" -eq 1
done
while read -r image expected_id; do
  test -n "${image}" -a -n "${expected_id}"
  actual_id="$(docker image inspect --format '{{.Id}}' "${image}")"
  test "${actual_id}" = "${expected_id}"
done <"${backup_root}/rollback-image-ids"
if [[ "${rollback_force_recreate}" == true ]]; then
  "${rollback_compose[@]}" up -d --force-recreate --no-build \
    --wait --wait-timeout 600
else
  "${rollback_compose[@]}" up -d --no-build --wait --wait-timeout 600
fi
curl --fail --silent --show-error \
  https://185.99.135.224:7860/api/health
```

The TLS archive is normally a safety copy rather than part of an application
rollback: a newly renewed, still-valid certificate should be kept. If the live
certificate tree itself was damaged, stop gateway and Certbot, move the damaged
tree aside, restore `tls.tar.gz` under `${PDF2ZH_SERVICE_ROOT}`, validate the IP
SAN/key pair with the accepted gateway image, and only then restart them. Never
extract it over a live Certbot process.

### Operator access and provider policy

The production UI is `https://185.99.135.224:7860/`; the admin page is
`https://185.99.135.224:7860/admin`. Retrieve the one-time setup token over SSH
from `/docker/pdf2zh-rust/secrets/admin-setup-token`. It is accepted only while
no administrator exists. Do not transmit it through chat, logs, command-line
arguments, or a browser URL.

The provider master key must always be backed up with `web-state`. Losing it
makes encrypted provider credentials undecryptable, and startup fails closed.
The bootstrap script will not recreate a missing key unless the explicit
first-key-initialization flag is present. It is valid for the one-time migration
from a pre-admin deployment, but never use it to replace a lost key after an
admin-capable release has stored encrypted provider state.

DeepSeek credentials are not read from `PDF2ZH_DEEPSEEK_API_KEY`. An
administrator enters them in the backend. DeepSeek uses the fixed official
`https://api.deepseek.com` endpoint and the `deepseek-v4-flash` or
`deepseek-v4-pro` model. Administrators may also define HTTPS OpenAI-compatible
providers and model allow-lists. Google remains available without a secret.

DeepSeek and other credentialed providers are selectable only in an
authenticated administrator session; anonymous users remain restricted to
Google. Admin authentication protects both provider configuration and paid
translation submission. Provider credentials still enter the per-job
translation sandbox at runtime, so administrators should not submit untrusted
PDFs to a credentialed provider.

The former `translator-cache` and `translator-config` volumes are deliberately
not mounted. Archive and audit them as shown above; do not silently delete them.
The production defaults remain one concurrent translation, a 100 MiB upload
limit, a 180-second browser lease, a 15-second reload grace, and six-hour
artifact retention. `PDF2ZH_WORKER_TOKEN` has no fallback.

Finally, HTTP cookies are scoped by host and path, not by TCP port. While using
a raw IP, do not expose an unrelated or untrusted HTTPS application on another
port of `185.99.135.224`; use a dedicated domain or IP if the host later serves
multiple applications.
