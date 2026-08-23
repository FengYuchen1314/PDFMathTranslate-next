#!/usr/bin/env bash
set -euo pipefail

ops_image="${1:-pdf2zh-ops:test}"
portable_root="$(mktemp -d)"
test -n "${portable_root}" && test "${portable_root}" != "/"

cleanup() {
  sudo rm -rf -- "${portable_root}"
}
trap cleanup EXIT

mkdir -p \
  "${portable_root}/data/jobs" \
  "${portable_root}/data/state" \
  "${portable_root}/secrets/web" \
  "${portable_root}/secrets/worker"

run_init() {
  docker run --rm \
    --network none \
    --cap-drop ALL \
    --cap-add CHOWN \
    --cap-add DAC_OVERRIDE \
    --cap-add FOWNER \
    -v "${portable_root}/data/jobs:/data/jobs" \
    -v "${portable_root}/data/state:/data/state" \
    -v "${portable_root}/secrets/web:/run/pdf2zh-init-secrets/web" \
    -v "${portable_root}/secrets/worker:/run/pdf2zh-init-secrets/worker" \
    "${ops_image}"
}

run_init
sudo test -s "${portable_root}/secrets/web/provider-master-key"
sudo test -s "${portable_root}/secrets/web/admin-setup-token"
sudo cmp \
  "${portable_root}/secrets/web/worker-token" \
  "${portable_root}/secrets/worker/token"
test "$(sudo stat -c '%a' "${portable_root}/data/jobs")" = 700
test "$(sudo stat -c '%a' "${portable_root}/data/state")" = 700
test "$(sudo stat -c '%a' \
  "${portable_root}/secrets/web/provider-master-key")" = 400

secret_hashes_before="$(
  sudo sha256sum \
    "${portable_root}/secrets/web/provider-master-key" \
    "${portable_root}/secrets/web/admin-setup-token" \
    "${portable_root}/secrets/web/worker-token" \
    "${portable_root}/secrets/worker/token"
)"
run_init
secret_hashes_after="$(
  sudo sha256sum \
    "${portable_root}/secrets/web/provider-master-key" \
    "${portable_root}/secrets/web/admin-setup-token" \
    "${portable_root}/secrets/web/worker-token" \
    "${portable_root}/secrets/worker/token"
)"
test "${secret_hashes_before}" = "${secret_hashes_after}"

sudo mv \
  "${portable_root}/secrets/web/provider-master-key" \
  "${portable_root}/provider-master-key.saved"
printf 'persisted state\n' \
  | sudo tee "${portable_root}/data/state/recovery-guard" >/dev/null
if run_init; then
  echo 'init replaced a lost master key while state existed' >&2
  exit 1
fi
sudo mv \
  "${portable_root}/provider-master-key.saved" \
  "${portable_root}/secrets/web/provider-master-key"

sudo cp \
  "${portable_root}/secrets/worker/token" \
  "${portable_root}/translator-worker-token.saved"
printf 'mismatch\n' \
  | sudo tee "${portable_root}/secrets/worker/token" >/dev/null
if run_init; then
  echo 'init accepted mismatched worker tokens' >&2
  exit 1
fi
sudo mv \
  "${portable_root}/translator-worker-token.saved" \
  "${portable_root}/secrets/worker/token"

sudo mv \
  "${portable_root}/secrets/web/worker-token" \
  "${portable_root}/web-worker-token.saved"
sudo mv \
  "${portable_root}/secrets/worker/token" \
  "${portable_root}/translator-worker-token.saved"
if run_init; then
  echo 'init replaced both lost worker tokens while state existed' >&2
  exit 1
fi

echo 'portable_init_contract=passed'
