#!/usr/bin/env bash
set -euo pipefail

readonly service_ip="${PDF2ZH_PUBLIC_IP:-185.99.135.224}"
readonly service_root="${PDF2ZH_SERVICE_ROOT:-/docker/pdf2zh-rust}"
readonly secret_root="${PDF2ZH_SECRET_ROOT:-${service_root}/secrets}"
readonly letsencrypt_root="${PDF2ZH_LETSENCRYPT_ROOT:-${service_root}/tls/letsencrypt}"
readonly acme_webroot="${PDF2ZH_ACME_WEBROOT:-${service_root}/tls/www}"
readonly master_key_file="${secret_root}/provider-master-key"
readonly setup_token_file="${secret_root}/admin-setup-token"
readonly certificate_file="${letsencrypt_root}/live/${service_ip}/fullchain.pem"
readonly private_key_file="${letsencrypt_root}/live/${service_ip}/privkey.pem"
readonly renewal_file="${letsencrypt_root}/renewal/${service_ip}.conf"
readonly bootstrap_container="pdf2zh-cert-bootstrap"

if [[ "${EUID}" -ne 0 ]]; then
    echo "Run this script as root on the target VPS." >&2
    exit 1
fi

command -v docker >/dev/null
command -v openssl >/dev/null
command -v ss >/dev/null
command -v curl >/dev/null

install -d -m 0700 "${secret_root}" "${letsencrypt_root}"
install -d -m 0755 "${acme_webroot}"
umask 077

if [[ ! -s "${master_key_file}" ]]; then
    if [[ "${PDF2ZH_INITIALIZE_MASTER_KEY:-false}" != "true" ]]; then
        echo "Provider master key is missing; refusing to create a replacement." >&2
        echo "For a confirmed first key initialization (including a verified pre-admin migration)," >&2
        echo "rerun with PDF2ZH_INITIALIZE_MASTER_KEY=true." >&2
        exit 1
    fi
    openssl rand -base64 32 >"${master_key_file}"
fi
if [[ ! -s "${setup_token_file}" ]]; then
    openssl rand -base64 24 | tr -d '\n' >"${setup_token_file}"
fi

# File-backed Compose secrets retain their host UID/mode. The web image runs as
# UID 10001, so make it the sole non-root reader without widening permissions.
chown 10001:10001 "${master_key_file}" "${setup_token_file}"
chmod 0600 "${master_key_file}" "${setup_token_file}"

decoded_key_bytes="$(openssl base64 -d -A -in "${master_key_file}" | wc -c)"
if [[ "${decoded_key_bytes}" -ne 32 ]]; then
    echo "Existing provider master key is not a base64-encoded 32-byte value." >&2
    exit 1
fi

certificate_matches_key() {
    [[ -r "${certificate_file}" && -r "${private_key_file}" ]] || return 1
    local certificate_public_key private_public_key
    certificate_public_key="$(
        openssl x509 -in "${certificate_file}" -pubkey -noout \
            | openssl pkey -pubin -outform DER 2>/dev/null \
            | openssl sha256
    )"
    private_public_key="$(
        openssl pkey -in "${private_key_file}" -pubout -outform DER 2>/dev/null \
            | openssl sha256
    )"
    [[ -n "${certificate_public_key}" && "${certificate_public_key}" == "${private_public_key}" ]]
}

certificate_is_current=false
if [[ -r "${certificate_file}" && -r "${renewal_file}" ]] \
    && openssl x509 -in "${certificate_file}" -noout -checkend 86400 >/dev/null \
    && openssl x509 -in "${certificate_file}" -noout -ext subjectAltName \
        | grep -Fq "IP Address:${service_ip}" \
    && certificate_matches_key; then
    certificate_is_current=true
fi
if [[ "${certificate_is_current}" == true ]]; then
    echo "TLS certificate is valid for this IP; keeping it for automatic renewal."
    exit 0
fi

if docker ps -a --format '{{.Names}}' | grep -Fxq "${bootstrap_container}"; then
    echo "Refusing to replace existing container ${bootstrap_container}." >&2
    exit 1
fi

started_bootstrap=false
challenge_name="pdf2zh-bootstrap-$(openssl rand -hex 8)"
challenge_directory="${acme_webroot}/.well-known/acme-challenge"
challenge_file="${challenge_directory}/${challenge_name}"
challenge_value="pdf2zh-bootstrap-ok-${challenge_name}"
install -d -m 0755 "${challenge_directory}"
printf '%s' "${challenge_value}" >"${challenge_file}"
chmod 0644 "${challenge_file}"
cleanup() {
    rm -f -- "${challenge_file}"
    if [[ "${started_bootstrap}" == true ]]; then
        docker stop --time 5 "${bootstrap_container}" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

# An existing gateway can serve this same webroot during repair/renewal. Only
# start the temporary challenge server when the host's port 80 is actually free.
if ! ss -ltnH 'sport = :80' | grep -q .; then
    docker run --rm -d \
        --name "${bootstrap_container}" \
        -p 80:80 \
        -v "${acme_webroot}:/www:ro" \
        busybox:1.37.0@sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0 \
        httpd -f -p 80 -h /www >/dev/null
    started_bootstrap=true
fi

served_challenge="$(
    curl --fail --silent --show-error --max-time 5 \
        --header "Host: ${service_ip}" \
        "http://127.0.0.1/.well-known/acme-challenge/${challenge_name}"
)" || {
    echo "Port 80 is occupied but does not serve the configured ACME webroot." >&2
    exit 1
}
if [[ "${served_challenge}" != "${challenge_value}" ]]; then
    echo "Port 80 returned the wrong ACME challenge content." >&2
    exit 1
fi

certbot_args=(
    certonly
    --preferred-profile shortlived
    --webroot
    --webroot-path /var/www/certbot
    --ip-address "${service_ip}"
    --cert-name "${service_ip}"
    --non-interactive
    --agree-tos
)
if [[ -f "${certificate_file}" ]]; then
    certbot_args+=(--force-renewal)
fi
if [[ -n "${LETSENCRYPT_EMAIL:-}" ]]; then
    certbot_args+=(--email "${LETSENCRYPT_EMAIL}")
else
    certbot_args+=(--register-unsafely-without-email)
fi

docker run --rm \
    -v "${letsencrypt_root}:/etc/letsencrypt" \
    -v "${acme_webroot}:/var/www/certbot" \
    certbot/certbot:v5.7.0@sha256:34ee91d2f43008eb78a007d22f23ed4b2eaa9a454cb27ca2c042b49527a695b4 \
    "${certbot_args[@]}"

openssl x509 -in "${certificate_file}" -noout -checkend 86400 >/dev/null
openssl x509 -in "${certificate_file}" -noout -ext subjectAltName \
    | grep -Fq "IP Address:${service_ip}"
test -r "${renewal_file}"
certificate_matches_key
echo "VPS secrets and the short-lived IP certificate are ready."
echo "Retrieve the one-time setup token over SSH from: ${setup_token_file}"
