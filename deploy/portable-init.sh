#!/bin/sh
set -eu

umask 077

service_ip="${PDF2ZH_PUBLIC_IP:-185.99.135.224}"
jobs_root="/data/jobs"
state_root="/data/state"
web_secret_root="/run/pdf2zh-init-secrets/web"
worker_secret_root="/run/pdf2zh-init-secrets/worker"
active_tls_root="/run/pdf2zh-init-tls/active"
letsencrypt_root="/run/pdf2zh-init-tls/letsencrypt"
acme_root="/run/pdf2zh-init-tls/acme"
certbot_state_root="/var/lib/letsencrypt"
trusted_marker="${active_tls_root}/trusted-ready"

fail() {
    echo "portable initialization failed: $*" >&2
    exit 1
}

valid_ipv4() {
    case "${service_ip}" in
        ''|*[!0-9.]*) return 1 ;;
    esac
    old_ifs="${IFS}"
    IFS=.
    set -- ${service_ip}
    IFS="${old_ifs}"
    [ "$#" -eq 4 ] || return 1
    for octet in "$@"; do
        case "${octet}" in
            ''|*[!0-9]*) return 1 ;;
        esac
        [ "${octet}" -le 255 ] || return 1
    done
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
    openssl rand -base64 "${byte_count}" | tr -d '\r\n' >"${temporary}"
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
    decoded_bytes="$(
        openssl base64 -d -A -in "${secret_file}" 2>/dev/null | wc -c | tr -d ' '
    )" || return 1
    [ "${decoded_bytes}" = "${expected_bytes}" ]
}

certificate_matches_key() {
    certificate="$1"
    private_key="$2"
    [ -s "${certificate}" ] && [ -s "${private_key}" ] || return 1
    certificate_public_key="$(
        openssl x509 -in "${certificate}" -pubkey -noout 2>/dev/null \
            | openssl pkey -pubin -outform DER 2>/dev/null \
            | openssl sha256
    )" || return 1
    private_public_key="$(
        openssl pkey -in "${private_key}" -pubout -outform DER 2>/dev/null \
            | openssl sha256
    )" || return 1
    [ -n "${certificate_public_key}" ] \
        && [ "${certificate_public_key}" = "${private_public_key}" ]
}

certificate_has_service_ip() {
    certificate="$1"
    openssl x509 -in "${certificate}" -noout -ext subjectAltName 2>/dev/null \
        | tr ',' '\n' \
        | sed 's/^[[:space:]]*//' \
        | grep -Fxq "IP Address:${service_ip}"
}

active_certificate_is_usable() {
    certificate="${active_tls_root}/fullchain.pem"
    private_key="${active_tls_root}/privkey.pem"
    openssl x509 -in "${certificate}" -noout -checkend 3600 >/dev/null 2>&1 \
        && certificate_has_service_ip "${certificate}" \
        && certificate_matches_key "${certificate}" "${private_key}"
}

trusted_lineage_matches_active() {
    lineage_certificate="${letsencrypt_root}/live/${service_ip}/fullchain.pem"
    lineage_private_key="${letsencrypt_root}/live/${service_ip}/privkey.pem"
    renewal_file="${letsencrypt_root}/renewal/${service_ip}.conf"
    [ -s "${trusted_marker}" ] \
        && [ -s "${renewal_file}" ] \
        && openssl x509 -in "${lineage_certificate}" -noout -checkend 3600 >/dev/null 2>&1 \
        && certificate_matches_key "${lineage_certificate}" "${lineage_private_key}" \
        && [ "$(openssl x509 -sha256 -fingerprint -noout -in "${lineage_certificate}")" \
            = "$(openssl x509 -sha256 -fingerprint -noout -in "${active_tls_root}/fullchain.pem")" ]
}

create_bootstrap_certificate() {
    certificate_temporary="${active_tls_root}/.fullchain.pem.tmp.$$"
    private_key_temporary="${active_tls_root}/.privkey.pem.tmp.$$"
    rm -f -- "${trusted_marker}"
    rm -f -- "${certificate_temporary}" "${private_key_temporary}"
    openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 7 \
        -subj "/CN=${service_ip}" \
        -addext "subjectAltName=IP:${service_ip}" \
        -keyout "${private_key_temporary}" \
        -out "${certificate_temporary}" >/dev/null 2>&1
    certificate_matches_key "${certificate_temporary}" "${private_key_temporary}" \
        || fail "generated TLS certificate does not match its private key"
    chmod 0600 "${private_key_temporary}"
    chmod 0644 "${certificate_temporary}"
    mv -f -- "${private_key_temporary}" "${active_tls_root}/privkey.pem"
    mv -f -- "${certificate_temporary}" "${active_tls_root}/fullchain.pem"
}

valid_ipv4 || fail "PDF2ZH_PUBLIC_IP must be an IPv4 address"

mkdir -p \
    "${jobs_root}" \
    "${state_root}" \
    "${web_secret_root}" \
    "${worker_secret_root}" \
    "${active_tls_root}" \
    "${letsencrypt_root}" \
    "${acme_root}/.well-known/acme-challenge" \
    "${certbot_state_root}"

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

chown -R 0:0 \
    "${active_tls_root}" \
    "${letsencrypt_root}" \
    "${acme_root}" \
    "${certbot_state_root}"
chmod 0755 "${active_tls_root}"
chmod 0700 "${letsencrypt_root}" "${certbot_state_root}"
chmod 0755 "${acme_root}" "${acme_root}/.well-known" \
    "${acme_root}/.well-known/acme-challenge"

if ! active_certificate_is_usable; then
    create_bootstrap_certificate
elif ! trusted_lineage_matches_active; then
    rm -f -- "${trusted_marker}"
fi

echo "Portable storage, credentials, and bootstrap TLS are ready."
echo "After startup, read ./secrets/web/admin-setup-token on the host to claim the first administrator account."
