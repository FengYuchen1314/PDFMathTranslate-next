#!/bin/sh
set -eu

umask 077

jobs_root="/data/jobs"
state_root="/data/state"
web_secret_root="/run/pdf2zh-init-secrets/web"
worker_secret_root="/run/pdf2zh-init-secrets/worker"

fail() {
    echo "portable initialization failed: $*" >&2
    exit 1
}

directory_has_state() {
    for candidate in \
        "${state_root}"/* \
        "${state_root}"/.[!.]* \
        "${state_root}"/..?*; do
        if [ -e "${candidate}" ]; then
            return 0
        fi
    done
    return 1
}

generate_secret() {
    destination="$1"
    byte_count="$2"
    temporary="$(mktemp "${destination}.tmp.XXXXXX")"
    head -c "${byte_count}" /dev/urandom | base64 | tr -d '\r\n' >"${temporary}"
    [ -s "${temporary}" ] || fail "could not generate ${destination}"
    mv -f -- "${temporary}" "${destination}"
}

copy_secret_atomically() {
    source="$1"
    destination="$2"
    temporary="$(mktemp "${destination}.tmp.XXXXXX")"
    cp -- "${source}" "${temporary}"
    [ -s "${temporary}" ] || fail "could not copy ${destination}"
    mv -f -- "${temporary}" "${destination}"
}

validate_base64_bytes() {
    secret_file="$1"
    expected_bytes="$2"
    decoded_file="$(mktemp "${secret_file}.decoded.XXXXXX")"
    if ! base64 -d <"${secret_file}" >"${decoded_file}" 2>/dev/null; then
        rm -f -- "${decoded_file}"
        return 1
    fi
    decoded_bytes="$(wc -c <"${decoded_file}" | tr -d ' ')"
    canonical="$(base64 <"${decoded_file}" | tr -d '\r\n')"
    supplied="$(tr -d '\r\n' <"${secret_file}")"
    rm -f -- "${decoded_file}"
    [ "${decoded_bytes}" = "${expected_bytes}" ] && [ "${canonical}" = "${supplied}" ]
}

mkdir -p \
    "${jobs_root}" \
    "${state_root}" \
    "${web_secret_root}" \
    "${worker_secret_root}"

master_key_file="${web_secret_root}/provider-master-key"
setup_token_file="${web_secret_root}/admin-setup-token"
web_worker_token_file="${web_secret_root}/worker-token"
worker_token_file="${worker_secret_root}/token"

if [ ! -s "${master_key_file}" ]; then
    if directory_has_state; then
        fail "provider master key is missing while application state exists"
    fi
    generate_secret "${master_key_file}" 32
fi
validate_base64_bytes "${master_key_file}" 32 \
    || fail "provider master key must decode to exactly 32 bytes"

if [ ! -s "${setup_token_file}" ]; then
    if directory_has_state; then
        fail "administrator setup token is missing while application state exists"
    fi
    generate_secret "${setup_token_file}" 24
fi
validate_base64_bytes "${setup_token_file}" 24 \
    || fail "administrator setup token must decode to exactly 24 bytes"

if [ -s "${web_worker_token_file}" ] && [ -s "${worker_token_file}" ]; then
    cmp -s "${web_worker_token_file}" "${worker_token_file}" \
        || fail "web and translator worker tokens do not match"
elif [ -s "${web_worker_token_file}" ]; then
    copy_secret_atomically "${web_worker_token_file}" "${worker_token_file}"
elif [ -s "${worker_token_file}" ]; then
    copy_secret_atomically "${worker_token_file}" "${web_worker_token_file}"
else
    if directory_has_state; then
        fail "both worker-token copies are missing while application state exists"
    fi
    generate_secret "${web_worker_token_file}" 32
    copy_secret_atomically "${web_worker_token_file}" "${worker_token_file}"
fi
validate_base64_bytes "${web_worker_token_file}" 32 \
    || fail "web worker token must decode to exactly 32 bytes"
validate_base64_bytes "${worker_token_file}" 32 \
    || fail "translator worker token must decode to exactly 32 bytes"

chown 10001:10001 "${jobs_root}" "${state_root}"
chmod 0700 "${jobs_root}" "${state_root}"

chown 0:10001 "${web_secret_root}"
chmod 0710 "${web_secret_root}"
chown 10001:10001 \
    "${master_key_file}" \
    "${setup_token_file}" \
    "${web_worker_token_file}"
chmod 0400 \
    "${master_key_file}" \
    "${setup_token_file}" \
    "${web_worker_token_file}"

chown 0:0 "${worker_secret_root}" "${worker_token_file}"
chmod 0700 "${worker_secret_root}"
chmod 0400 "${worker_token_file}"

echo "Portable storage and credentials are ready."
echo "Read ./secrets/web/admin-setup-token on the host to claim the first administrator account."
