#!/bin/sh
set -eu

marker_path="/var/lib/letsencrypt/pdf2zh-last-success"
marker_temporary="${marker_path}.tmp.$$"
active_pid=""

shutdown() {
    trap - TERM INT
    if [ -n "${active_pid}" ]; then
        kill "${active_pid}" 2>/dev/null || true
        wait "${active_pid}" 2>/dev/null || true
    fi
    rm -f -- "${marker_temporary}"
    exit 0
}
trap shutdown TERM INT

rm -f -- "${marker_path}" "${marker_temporary}"
failure_delay=60

while :; do
    certbot renew --quiet --no-random-sleep-on-renew &
    active_pid=$!
    if wait "${active_pid}"; then
        renew_succeeded=true
    else
        renew_succeeded=false
    fi
    active_pid=""

    if [ "${renew_succeeded}" = true ]; then
        date +%s >"${marker_temporary}"
        mv -f -- "${marker_temporary}" "${marker_path}"
        sleep_delay=43200
        failure_delay=60
    else
        sleep_delay="${failure_delay}"
        case "${failure_delay}" in
            60) failure_delay=300 ;;
            300) failure_delay=900 ;;
            900) failure_delay=1800 ;;
            1800) failure_delay=3600 ;;
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
        exit "${status}"
    fi
    active_pid=""
done
