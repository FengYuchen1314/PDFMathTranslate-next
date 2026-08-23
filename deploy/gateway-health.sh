#!/bin/sh
set -eu

service_ip="${PDF2ZH_PUBLIC_IP:-185.99.135.224}"
served_certificate="/tmp/pdf2zh-served-cert.pem"
active_certificate="/etc/pdf2zh-tls/fullchain.pem"

rm -f -- "${served_certificate}"
trap 'rm -f -- "${served_certificate}"' EXIT

certificate_has_service_ip() {
    certificate="$1"
    openssl x509 -in "${certificate}" -noout -ext subjectAltName 2>/dev/null \
        | tr ',' '\n' \
        | sed 's/^[[:space:]]*//' \
        | grep -Fxq "IP Address:${service_ip}"
}

openssl s_client \
    -connect 127.0.0.1:7860 \
    -servername "${service_ip}" \
    -showcerts </dev/null 2>/dev/null \
    | openssl x509 -out "${served_certificate}"
openssl x509 -checkend 300 -noout -in "${served_certificate}" >/dev/null
certificate_has_service_ip "${served_certificate}"
test "$(openssl x509 -sha256 -fingerprint -noout -in "${served_certificate}")" \
    = "$(openssl x509 -sha256 -fingerprint -noout -in "${active_certificate}")"
if [ -s /etc/pdf2zh-tls/trusted-ready ]; then
    curl --fail --silent --show-error --insecure --noproxy '*' \
        --header "Host: ${service_ip}" \
        https://127.0.0.1:7860/api/health >/dev/null
else
    status="$(
        curl --silent --show-error --insecure --noproxy '*' \
            --output /dev/null --write-out '%{http_code}' \
            --header "Host: ${service_ip}" \
            https://127.0.0.1:7860/
    )"
    test "${status}" = "503"
fi
