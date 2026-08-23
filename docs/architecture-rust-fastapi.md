# Rust + FastAPI Web Architecture

The public web application no longer runs on Gradio. It is split into two
containers and one browser application:

```text
Browser (React + Vite)
       │ HTTP / SSE
       ▼
Rust web service (public :7860)
  uploads · web-only SQLite state · fair queue · session leases · downloads
       │ private HTTP + shared /data/jobs volume
       ▼
FastAPI translator (private :8000, supervised Uvicorn child)
  GoogleSettings · do_translate_async_stream · BabelDOC process tree
```

Only the Rust service is published. The FastAPI port is exposed solely on the
Compose network and requires a shared worker token. Its request model has no
provider field and constructs `GoogleSettings()` directly; automatic LLM term
extraction is disabled.

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
- `POST /api/jobs` (multipart PDF and Google translation options; idempotent
  client UUID in `X-Job-Id`)
- `GET /api/jobs/{id}`
- `GET /api/jobs/{id}/events` (SSE)
- `POST /api/jobs/{id}/cancel`
- `POST /api/jobs/{id}/heartbeat`
- `POST /api/jobs/{id}/release`
- `DELETE /api/jobs/{id}` (explicit terminal cleanup)
- `GET /api/jobs/{id}/files/{mono|dual|glossary}`

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

## VPS deployment

Run all validation and builds on the target Linux host:

```bash
python -m pytest -q tests/test_worker_api.py \
  tests/test_worker_supervisor.py tests/test_high_level_cancellation.py
python -m ruff format --check pdf2zh_next/worker_api.py \
  pdf2zh_next/worker_supervisor.py tests/test_worker_api.py
python -m ruff check pdf2zh_next/worker_api.py \
  pdf2zh_next/worker_supervisor.py tests/test_worker_api.py

cd backend-rs
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cd ..

cd frontend
npm ci
npm test
npm run build
cd ..

umask 077
printf 'PDF2ZH_WORKER_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
docker compose --env-file .env -f deploy/compose.vps.yml config --quiet
docker compose --env-file .env -f deploy/compose.vps.yml build
docker compose --env-file .env -f deploy/compose.vps.yml up -d

python script/vps_rust_api_regression.py \
  http://127.0.0.1:7860 long.pdf short.pdf --rounds 3 --test-lease
python script/vps_worker_failstop_regression.py \
  http://127.0.0.1:7860 long.pdf
```

`PDF2ZH_PORT` can publish a staging port during acceptance testing; set
`PDF2ZH_BIND_ADDRESS=127.0.0.1` so that staging is reachable only through the
VPS itself or an SSH tunnel. The production defaults are `0.0.0.0:7860`, one
concurrent translation, a 100 MiB upload
limit, a 180-second browser lease, a 15-second reload grace, and six-hour
successful artifact retention.
`PDF2ZH_WORKER_TOKEN` is required; the Compose file intentionally has no
well-known fallback secret. The worker dependency graph is pinned in
`deploy/requirements.worker.lock.txt`. Runtime memory/PID ceilings and Docker
log rotation bound resource growth. A one-shot storage initializer repairs UID 10001
ownership on the shared job and web state volumes before either service starts.
The translator mounts its cache first so Docker can copy the image's warmed
BabelDOC assets into a fresh cache volume.
