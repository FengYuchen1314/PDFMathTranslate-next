#!/usr/bin/env bash
set -Eeuo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "${script_dir}/.." && pwd -P)"
fixture_root="$(mktemp -d)"
case "${fixture_root}" in
  (/*) ;;
  (*) echo 'mktemp did not return an absolute fixture directory' >&2; exit 1 ;;
esac
test -n "${fixture_root}"
test "${fixture_root}" != /
cleanup() {
  rm -f -- \
    "${fixture_root}/bin/certbot" \
    "${fixture_root}/bin/sleep" \
    "${fixture_root}/attempts" \
    "${fixture_root}/delays" \
    "${fixture_root}/marker"
  rmdir -- "${fixture_root}/bin" "${fixture_root}"
}
trap cleanup EXIT
mkdir "${fixture_root}/bin"

cat >"${fixture_root}/bin/certbot" <<'SH'
#!/bin/sh
set -eu
attempts_file="${PDF2ZH_CERTBOT_TEST_ATTEMPTS:?}"
attempts=0
if [ -s "${attempts_file}" ]; then
    attempts="$(cat "${attempts_file}")"
fi
attempts=$((attempts + 1))
printf '%s\n' "${attempts}" >"${attempts_file}"
test "${attempts}" -eq 6
SH

cat >"${fixture_root}/bin/sleep" <<'SH'
#!/bin/sh
set -eu
delays_file="${PDF2ZH_CERTBOT_TEST_DELAYS:?}"
marker_path="${PDF2ZH_CERTBOT_MARKER_PATH:?}"
printf '%s\n' "$1" >>"${delays_file}"
delay_count="$(wc -l <"${delays_file}")"
if [ "${delay_count}" -lt 6 ]; then
    test ! -e "${marker_path}"
else
    test -s "${marker_path}"
fi
if [ "${delay_count}" -ge 7 ]; then
    kill -TERM "${PPID}"
fi
SH

chmod 0700 "${fixture_root}/bin/certbot" "${fixture_root}/bin/sleep"
printf '%s\n' stale >"${fixture_root}/marker"
export PDF2ZH_CERTBOT_TEST_ATTEMPTS="${fixture_root}/attempts"
export PDF2ZH_CERTBOT_TEST_DELAYS="${fixture_root}/delays"
export PDF2ZH_CERTBOT_MARKER_PATH="${fixture_root}/marker"

# Rewrite only the fixed production marker path for the isolated fixture. The
# loop logic itself, including its trap and delay state machine, is unchanged.
sed "s|/var/lib/letsencrypt/pdf2zh-last-success|${fixture_root}/marker|" \
  "${repo_root}/deploy/certbot-renew-loop.sh" \
  | PATH="${fixture_root}/bin:${PATH}" /bin/sh

test "$(<"${fixture_root}/attempts")" -eq 7
test "$(paste -sd, "${fixture_root}/delays")" = \
  '60,300,900,1800,3600,43200,60'
grep -Eq '^[0-9]+$' "${fixture_root}/marker"
echo 'certbot_renew_backoff_marker_and_term=passed'
