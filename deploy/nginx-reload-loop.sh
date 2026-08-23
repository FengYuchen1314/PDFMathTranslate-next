#!/bin/sh
set -eu

reload_interval="${PDF2ZH_NGINX_RELOAD_SECONDS:-300}"
certificate_path="/etc/letsencrypt/live/185.99.135.224/fullchain.pem"

certificate_fingerprint() {
    openssl x509 -sha256 -fingerprint -noout -in "$certificate_path"
}

reload_certificates() {
    active_fingerprint="$(certificate_fingerprint)"
    while sleep "$reload_interval"; do
        if candidate_fingerprint="$(certificate_fingerprint)"; then
            if [ "$candidate_fingerprint" = "$active_fingerprint" ]; then
                continue
            fi
            if nginx -t; then
                if nginx -s reload; then
                    active_fingerprint="$candidate_fingerprint"
                else
                    echo "nginx certificate reload failed; will retry" >&2
                fi
            else
                echo "nginx configuration check failed; will retry reload" >&2
            fi
        else
            echo "could not read renewed certificate; will retry" >&2
        fi
    done
}

reload_certificates &
exec nginx -g 'daemon off;'
