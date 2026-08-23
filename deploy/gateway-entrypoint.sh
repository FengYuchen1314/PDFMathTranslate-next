#!/bin/sh
set -eu

service_ip="${PDF2ZH_PUBLIC_IP:-185.99.135.224}"
public_port="${PDF2ZH_PORT:-7860}"
reload_seconds="${PDF2ZH_NGINX_RELOAD_SECONDS:-60}"

# The historical source-on-VPS compose bind-mounts a read-only nginx.conf and
# supplies its own command. Keep that recovery path usable while the portable
# root compose uses the baked template below.
if [ "$#" -gt 0 ] && [ ! -w /etc/nginx/nginx.conf ]; then
    exec "$@"
fi

fail() {
    echo "gateway startup failed: $*" >&2
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

valid_ipv4 || fail "PDF2ZH_PUBLIC_IP must be an IPv4 address"
case "${public_port}" in
    ''|*[!0-9]*) fail "PDF2ZH_PORT must be an integer" ;;
esac
[ "${public_port}" -ge 1 ] && [ "${public_port}" -le 65535 ] \
    || fail "PDF2ZH_PORT must be between 1 and 65535"
case "${reload_seconds}" in
    ''|*[!0-9]*) fail "PDF2ZH_NGINX_RELOAD_SECONDS must be an integer" ;;
esac
[ "${reload_seconds}" -ge 5 ] \
    || fail "PDF2ZH_NGINX_RELOAD_SECONDS must be at least 5"

export PDF2ZH_PUBLIC_IP="${service_ip}"
export PDF2ZH_PORT="${public_port}"
export PDF2ZH_NGINX_RELOAD_SECONDS="${reload_seconds}"
envsubst '${PDF2ZH_PUBLIC_IP} ${PDF2ZH_PORT}' \
    </etc/nginx/templates/pdf2zh.conf.template \
    >/etc/nginx/nginx.conf

if [ "$#" -gt 0 ]; then
    exec "$@"
fi
exec /usr/local/bin/pdf2zh-nginx-reload-loop
