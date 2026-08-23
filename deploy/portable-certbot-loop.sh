#!/bin/sh
set -eu

service_ip="${PDF2ZH_PUBLIC_IP:-185.99.135.224}"
active_tls_root="/run/pdf2zh-active-tls"
certificate_file="/etc/letsencrypt/live/${service_ip}/fullchain.pem"
private_key_file="/etc/letsencrypt/live/${service_ip}/privkey.pem"
renewal_file="/etc/letsencrypt/renewal/${service_ip}.conf"
marker_path="/var/lib/letsencrypt/pdf2zh-last-success"
marker_temporary="${marker_path}.tmp.$$"
served_certificate="/tmp/pdf2zh-gateway-cert.pem.$$"
trusted_marker="${active_tls_root}/trusted-ready"
trusted_marker_temporary="${active_tls_root}/.trusted-ready.tmp.$$"
active_pid=""

cleanup_temporaries() {
    rm -f -- \
        "${marker_temporary}" \
        "${served_certificate}" \
        "${trusted_marker_temporary}" \
        "${active_tls_root}/.fullchain.pem.tmp.$$" \
        "${active_tls_root}/.privkey.pem.tmp.$$"
}

shutdown() {
    trap - TERM INT
    if [ -n "${active_pid}" ]; then
        kill "${active_pid}" 2>/dev/null || true
        wait "${active_pid}" 2>/dev/null || true
    fi
    cleanup_temporaries
    exit 0
}
trap shutdown TERM INT

certificate_matches_key() {
    certificate="$1"
    private_key="$2"
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

lineage_is_usable() {
    [ -s "${certificate_file}" ] \
        && [ -s "${private_key_file}" ] \
        && [ -s "${renewal_file}" ] \
        && openssl x509 -in "${certificate_file}" -noout -checkend 3600 >/dev/null 2>&1 \
        && certificate_has_service_ip "${certificate_file}" \
        && certificate_matches_key "${certificate_file}" "${private_key_file}"
}

sync_active_certificate() {
    lineage_is_usable || return 1
    certificate_temporary="${active_tls_root}/.fullchain.pem.tmp.$$"
    private_key_temporary="${active_tls_root}/.privkey.pem.tmp.$$"
    cp -L -- "${certificate_file}" "${certificate_temporary}"
    cp -L -- "${private_key_file}" "${private_key_temporary}"
    certificate_matches_key "${certificate_temporary}" "${private_key_temporary}" || return 1
    chmod 0644 "${certificate_temporary}"
    chmod 0600 "${private_key_temporary}"
    mv -f -- "${private_key_temporary}" "${active_tls_root}/privkey.pem"
    mv -f -- "${certificate_temporary}" "${active_tls_root}/fullchain.pem"
}

gateway_serves_active_certificate() {
    rm -f -- "${served_certificate}"
    openssl s_client \
        -connect gateway:7860 \
        -servername "${service_ip}" \
        -showcerts </dev/null 2>/dev/null \
        | openssl x509 -out "${served_certificate}" >/dev/null 2>&1 \
        || return 1
    [ "$(openssl x509 -sha256 -fingerprint -noout -in "${served_certificate}")" \
        = "$(openssl x509 -sha256 -fingerprint -noout -in "${active_tls_root}/fullchain.pem")" ]
}

wait_for_gateway_reload() {
    deadline="$(( $(date +%s) + 180 ))"
    while [ "$(date +%s)" -lt "${deadline}" ]; do
        if gateway_serves_active_certificate; then
            rm -f -- "${served_certificate}"
            return 0
        fi
        sleep 2 &
        active_pid=$!
        if wait "${active_pid}"; then
            active_pid=""
        else
            active_pid=""
            return 1
        fi
    done
    echo "gateway did not load the renewed certificate within 180 seconds" >&2
    return 1
}

request_or_renew_certificate() {
    if [ -s "${renewal_file}" ]; then
        certbot renew \
            --cert-name "${service_ip}" \
            --quiet \
            --no-random-sleep-on-renew
        return
    fi

    set -- \
        certonly \
        --preferred-profile shortlived \
        --webroot \
        --webroot-path /var/www/certbot \
        --ip-address "${service_ip}" \
        --cert-name "${service_ip}" \
        --non-interactive \
        --agree-tos
    if [ -n "${LETSENCRYPT_EMAIL:-}" ]; then
        set -- "$@" --email "${LETSENCRYPT_EMAIL}"
    else
        set -- "$@" --register-unsafely-without-email
    fi
    certbot "$@"
}

rm -f -- "${marker_path}" "${marker_temporary}"
failure_delay=60

while :; do
    request_or_renew_certificate &
    active_pid=$!
    if wait "${active_pid}"; then
        certbot_succeeded=true
    else
        certbot_succeeded=false
    fi
    active_pid=""
    if [ "${certbot_succeeded}" = true ] \
        && sync_active_certificate \
        && wait_for_gateway_reload; then
        openssl x509 -sha256 -fingerprint -noout \
            -in "${active_tls_root}/fullchain.pem" >"${trusted_marker_temporary}"
        chmod 0644 "${trusted_marker_temporary}"
        mv -f -- "${trusted_marker_temporary}" "${trusted_marker}"
        date +%s >"${marker_temporary}"
        mv -f -- "${marker_temporary}" "${marker_path}"
        sleep_delay=43200
        failure_delay=60
    else
        echo "certificate issue or renewal failed; retrying after ${failure_delay} seconds" >&2
        sleep_delay="${failure_delay}"
        case "${failure_delay}" in
            60) failure_delay=300 ;;
            300) failure_delay=900 ;;
            900) failure_delay=1800 ;;
            *) failure_delay=3600 ;;
        esac
    fi
    sleep "${sleep_delay}" &
    active_pid=$!
    if wait "${active_pid}"; then
        :
    else
        status=$?
        active_pid=""
        cleanup_temporaries
        exit "${status}"
    fi
    active_pid=""
done
