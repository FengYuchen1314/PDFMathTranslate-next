#!/usr/bin/env bash
set -Eeuo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "${script_dir}/.." && pwd -P)"
runbook="${repo_root}/docs/architecture-rust-fastapi.md"

# Every fenced Bash block must at least parse independently. This catches a
# broken heredoc, array, or continuation before an operator reaches cutover.
python3 - "${runbook}" <<'PY'
import pathlib
import re
import subprocess
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
blocks = re.findall(r"```bash\n(.*?)\n```", text, flags=re.DOTALL)
if not blocks:
    raise SystemExit("runbook contains no Bash fixtures")
for index, block in enumerate(blocks, start=1):
    result = subprocess.run(
        ["bash", "-n"], input=block, text=True, capture_output=True, check=False
    )
    if result.returncode:
        raise SystemExit(f"runbook Bash block {index} is invalid: {result.stderr}")

normalized = re.sub(r"\s+", " ", text.replace("\\\n", " "))
first_install = (
    'atomic_release_env /dev/null "${PDF2ZH_PROD_ENV}" '
    '"${PDF2ZH_RELEASE_ID}" "${PDF2ZH_RELEASE_ID}" candidate'
)
if first_install not in normalized:
    raise SystemExit("first-install release-env example is not the five-argument call")
for required in (
    'if [[ -d "${old_workdir}/deploy" ]]',
    'elif [[ "${old_workdir%/}" == */deploy ]]',
    'old_source_root="${old_workdir%/deploy}"',
    'tar -C "${old_source_root}"',
    "printf 'PDF2ZH_LEASE_SECONDS=15\\n'",
    '--test-lease --lease-pdf "${PDF2ZH_LEASE_TEST_PDF}"',
):
    if required not in text:
        raise SystemExit(f"runbook lacks legacy working-dir handling: {required}")
PY

fixture_root="$(mktemp -d)"
case "${fixture_root}" in
  (/*) ;;
  (*) echo 'mktemp did not return an absolute fixture directory' >&2; exit 1 ;;
esac
test -n "${fixture_root}"
test "${fixture_root}" != /
cleanup() {
  rm -f -- \
    "${fixture_root}/hardcoded.yml" \
    "${fixture_root}/hardcoded.json" \
    "${fixture_root}/legacy.json" \
    "${fixture_root}/master-key" \
    "${fixture_root}/setup-token" \
    "${fixture_root}/candidate.env" \
    "${fixture_root}/backup/production.env" \
    "${fixture_root}/backup/rollback.env"
  rmdir -- "${fixture_root}/backup" "${fixture_root}"
}
trap cleanup EXIT

mkdir "${fixture_root}/backup"
printf '%s\n' \
  'PDF2ZH_WORKER_TOKEN=legacy-worker-token' \
  'PDF2ZH_DEEPSEEK_API_KEY=legacy-provider-key' \
  'PDF2ZH_PUBLIC_ORIGIN=http://185.99.135.224:7860' \
  'PDF2ZH_SECURE_COOKIES=false' \
  >"${fixture_root}/backup/production.env"

# Execute the exact atomic writer block extracted from the runbook against a
# legacy fixture. Rollback must preserve old settings; candidate must remove
# the legacy key and force the HTTPS cookie boundary.
export backup_root="${fixture_root}/backup"
export PDF2ZH_PROD_ENV="${fixture_root}/candidate.env"
export rollback_tag="rollback-fixture"
export rollback_gateway_tag="candidate-fixture"
export PDF2ZH_RELEASE_ID="candidate-fixture"
{
  printf '%s\n' 'set -Eeuo pipefail'
  python3 - "${runbook}" <<'PY'
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
blocks = re.findall(r"```bash\n(.*?)\n```", text, flags=re.DOTALL)
matches = [block for block in blocks if "atomic_release_env()" in block]
if len(matches) != 1:
    raise SystemExit(f"expected one atomic release-env block, found {len(matches)}")
print(matches[0])
PY
} | bash

grep -Fxq 'PDF2ZH_DEEPSEEK_API_KEY=legacy-provider-key' \
  "${fixture_root}/backup/rollback.env"
grep -Fxq 'PDF2ZH_PUBLIC_ORIGIN=http://185.99.135.224:7860' \
  "${fixture_root}/backup/rollback.env"
grep -Fxq 'PDF2ZH_SECURE_COOKIES=false' \
  "${fixture_root}/backup/rollback.env"
grep -Fxq 'PDF2ZH_WORKER_TOKEN=legacy-worker-token' \
  "${fixture_root}/backup/rollback.env"
! grep -q '^PDF2ZH_DEEPSEEK_API_KEY=' "${fixture_root}/candidate.env"
grep -Fxq 'PDF2ZH_PUBLIC_ORIGIN=https://185.99.135.224:7860' \
  "${fixture_root}/candidate.env"
grep -Fxq 'PDF2ZH_SECURE_COOKIES=true' "${fixture_root}/candidate.env"
candidate_worker_token="$(sed -n 's/^PDF2ZH_WORKER_TOKEN=//p' \
  "${fixture_root}/candidate.env")"
test -n "${candidate_worker_token}"
test "${candidate_worker_token}" != legacy-worker-token
unset candidate_worker_token

printf '%s\n' \
  'services:' \
  '  web:' \
  '    image: pdf2zh-next-web:latest' \
  '  translator:' \
  '    image: pdf2zh-next-translator:latest' \
  '  gateway:' \
  '    image: pdf2zh-next-gateway:latest' \
  >"${fixture_root}/hardcoded.yml"

export PDF2ZH_ROLLBACK_APP_IMAGE_TAG=rollback-fixture
export PDF2ZH_ROLLBACK_GATEWAY_IMAGE_TAG=candidate-fixture
docker compose -p pdf2zh-rollback-image-fixture \
  -f "${fixture_root}/hardcoded.yml" \
  -f "${repo_root}/deploy/compose.rollback-images.yml" \
  config --format json >"${fixture_root}/hardcoded.json"
python3 - "${fixture_root}/hardcoded.json" <<'PY'
import json
import pathlib
import sys

services = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["services"]
expected = {
    "web": "pdf2zh-next-web:rollback-fixture",
    "translator": "pdf2zh-next-translator:rollback-fixture",
    "gateway": "pdf2zh-next-gateway:candidate-fixture",
}
for service, image in expected.items():
    assert services[service]["image"] == image, services[service]
PY

export PDF2ZH_WORKER_TOKEN=fixture-worker-token
export PDF2ZH_LEGACY_TRANSLATOR_CACHE_VOLUME=fixture-translator-cache
export PDF2ZH_LEGACY_TRANSLATOR_CONFIG_VOLUME=fixture-translator-config
printf '%s\n' fixture-master-key >"${fixture_root}/master-key"
printf '%s\n' fixture-setup-token >"${fixture_root}/setup-token"
export PDF2ZH_MASTER_KEY_FILE_HOST="${fixture_root}/master-key"
export PDF2ZH_SETUP_TOKEN_FILE_HOST="${fixture_root}/setup-token"
/bin/sh -n "${repo_root}/deploy/certbot-renew-loop.sh"

# The capability-limited root supervisor must start without importing the
# package as root. Uvicorn performs the package import only after dropping to
# the unprivileged app user.
grep -Fxq \
  'CMD ["python", "/app/pdf2zh_next/worker_supervisor.py"]' \
  "${repo_root}/deploy/Dockerfile.worker"

docker compose -p pdf2zh-legacy-config-fixture --profile tls \
  -f "${repo_root}/deploy/compose.vps.yml" \
  -f "${repo_root}/deploy/compose.rollback-legacy-tls.yml" \
  -f "${repo_root}/deploy/compose.rollback-images.yml" \
  config --format json >"${fixture_root}/legacy.json"
python3 - "${fixture_root}/legacy.json" \
  "${repo_root}/deploy/certbot-renew-loop.sh" <<'PY'
import json
import pathlib
import sys

services = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["services"]
renew_loop = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8")
assert services["web"]["image"] == "pdf2zh-next-web:rollback-fixture"
assert services["translator"]["image"] == "pdf2zh-next-translator:rollback-fixture"
assert services["gateway"]["image"] == "pdf2zh-next-gateway:candidate-fixture"
assert not services["web"].get("ports"), services["web"].get("ports")

targets = {mount["target"] for mount in services["translator"]["volumes"]}
assert "/home/app/.cache" in targets, targets
assert "/home/app/.config/pdf2zh" in targets, targets
worker_health = " ".join(services["translator"]["healthcheck"]["test"])
assert "provider" in worker_health and "google" in worker_health, worker_health
for forbidden in ("max_active_jobs", "container_boot_id"):
    assert forbidden not in worker_health, worker_health

assert services["certbot"]["depends_on"]["gateway"]["condition"] == "service_healthy"
certbot_entrypoint = " ".join(services["certbot"]["entrypoint"])
assert "pdf2zh-certbot-renew-loop" in certbot_entrypoint, certbot_entrypoint
assert 'rm -f -- "${marker_path}"' in renew_loop
for transition in (
    "60) failure_delay=300",
    "300) failure_delay=900",
    "900) failure_delay=1800",
    "1800) failure_delay=3600",
    "*) failure_delay=3600",
):
    assert transition in renew_loop, transition
assert "sleep_delay=43200" in renew_loop
certbot_health = " ".join(services["certbot"]["healthcheck"]["test"])
assert "x509 -checkend 86400" in certbot_health, certbot_health
PY

echo 'deploy_config_and_runbook_shell_fixture=passed'
