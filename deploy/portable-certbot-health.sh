#!/bin/sh
set -eu

service_ip="${PDF2ZH_PUBLIC_IP:-185.99.135.224}"
certificate_file="/etc/letsencrypt/live/${service_ip}/fullchain.pem"
private_key_file="/etc/letsencrypt/live/${service_ip}/privkey.pem"
renewal_file="/etc/letsencrypt/renewal/${service_ip}.conf"
active_certificate="/run/pdf2zh-active-tls/fullchain.pem"
active_private_key="/run/pdf2zh-active-tls/privkey.pem"
marker_path="/var/lib/letsencrypt/pdf2zh-last-success"
served_certificate="/tmp/pdf2zh-health-gateway-cert.pem"
trusted_marker="/run/pdf2zh-active-tls/trusted-ready"

rm -f -- "${served_certificate}"
trap 'rm -f -- "${served_certificate}"' EXIT

certificate_has_service_ip() {
    certificate="$1"
    openssl x509 -in "${certificate}" -noout -ext subjectAltName 2>/dev/null \
        | tr ',' '\n' \
        | sed 's/^[[:space:]]*//' \
        | grep -Fxq "IP Address:${service_ip}"
}

test -s "${marker_path}"
test -s "${certificate_file}"
test -s "${private_key_file}"
test -s "${renewal_file}"
test -s "${active_certificate}"
test -s "${active_private_key}"
test -s "${trusted_marker}"
openssl x509 -checkend 3600 -noout -in "${certificate_file}" >/dev/null
certificate_has_service_ip "${certificate_file}"
test "$(openssl x509 -sha256 -fingerprint -noout -in "${certificate_file}")" \
    = "$(openssl x509 -sha256 -fingerprint -noout -in "${active_certificate}")"
certificate_public_key="$(
    openssl x509 -in "${active_certificate}" -pubkey -noout \
        | openssl pkey -pubin -outform DER 2>/dev/null \
        | openssl sha256
)"
private_public_key="$(
    openssl pkey -in "${active_private_key}" -pubout -outform DER 2>/dev/null \
        | openssl sha256
)"
test -n "${certificate_public_key}"
test "${certificate_public_key}" = "${private_public_key}"
openssl s_client \
    -connect gateway:7860 \
    -servername "${service_ip}" \
    -showcerts </dev/null 2>/dev/null \
    | openssl x509 -out "${served_certificate}" >/dev/null
test "$(openssl x509 -sha256 -fingerprint -noout -in "${served_certificate}")" \
    = "$(openssl x509 -sha256 -fingerprint -noout -in "${active_certificate}")"
test "$(cat "${trusted_marker}")" \
    = "$(openssl x509 -sha256 -fingerprint -noout -in "${active_certificate}")"
last_success="$(cat "${marker_path}")"
now="$(date +%s)"
test "$((now - last_success))" -lt 90000
