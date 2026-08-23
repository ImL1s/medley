#!/bin/sh
# Exercise install.sh against a fixture GitHub.
#
# Run from the repository root:  sh tests/installer/run_cases.sh
#
# The script under test hardcodes the GitHub hosts, deliberately — an
# environment variable that redirects where a piped-to-sh installer downloads
# from is a worse thing to have than an untested installer. So each case runs a
# copy with only the scheme-and-host rewritten, and the *real* hosts are covered
# separately by installing the actual published release in CI.
set -eu

PORT="${INSTALLER_TEST_PORT:-8747}"
REPO_ROOT="$(pwd)"
WORK="$(mktemp -d)"
# Must match the tag the fixture server serves.
VERSION='9.9.9+providers.1'
PASS=0
FAIL=0
SERVER_PID=''

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

ok() {
  PASS=$((PASS + 1))
  printf '  ok   %s\n' "$1"
}

bad() {
  FAIL=$((FAIL + 1))
  printf '  FAIL %s\n' "$1"
}

# The triple install.sh will detect on this machine.
#
# Asked of the script under test rather than reimplemented here. This used to
# be a copy of the detection, which was fine while a Linux host always meant
# `unknown-linux-gnu`; now that the Linux branch reads the host's glibc version
# and can choose musl (issue #82), a mirror that drifts would build a fixture
# archive under a triple the installer never asks for — and every case below
# would fail for a reason that has nothing to do with what it tests.
#
# `main` is disarmed so sourcing defines the functions without installing
# anything. This runs inside a command substitution, so nothing it defines
# escapes into this shell.
target_triple() {
  sed 's/^main "$@"$/:/' "${REPO_ROOT}/install.sh" > "${WORK}/install-functions.sh"
  if grep -q '^main "\$@"$' "${WORK}/install-functions.sh"; then
    echo 'error: main() is still armed in the copy of install.sh' >&2
    exit 1
  fi
  # shellcheck source=/dev/null
  . "${WORK}/install-functions.sh"
  detect_target
}

TARGET="$(target_triple)"
ARCHIVE="medley-${VERSION}-${TARGET}.tar.gz"
CHECKSUMS="medley-${VERSION}-checksums.txt"

# A release that behaves like a real one: an archive whose single directory
# holds an executable `medley` that answers --version, plus a matching
# checksums file.
build_fixture_release() {
  assets="$1"
  tamper="$2"
  stage="${WORK}/stage/medley-${VERSION}-${TARGET}"
  rm -rf "${WORK}/stage"
  mkdir -p "$stage" "$assets"
  cat > "${stage}/medley" <<'BIN'
#!/bin/sh
echo "medley 9.9.9+providers.1 (fixture)"
BIN
  chmod 755 "${stage}/medley"
  printf 'fixture\n' > "${stage}/LICENSE"
  (cd "${WORK}/stage" && tar -czf "${assets}/${ARCHIVE}" "medley-${VERSION}-${TARGET}")

  digest="$(sha256_of "${assets}/${ARCHIVE}")"
  if [ "$tamper" = tamper ]; then
    # A digest that is the right shape and the wrong value: the installer must
    # reject on comparison, not on parsing.
    digest="$(printf '%064d' 0)"
  fi
  printf '%s  %s\n' "$digest" "$ARCHIVE" > "${assets}/${CHECKSUMS}"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

start_server() {
  scenario="$1"
  assets="$2"
  python3 "${REPO_ROOT}/tests/installer/fixture_server.py" "$PORT" "$scenario" "$assets" &
  SERVER_PID=$!
  # Wait for it rather than sleeping a guess.
  i=0
  while [ "$i" -lt 50 ]; do
    if curl -s -o /dev/null "http://127.0.0.1:${PORT}/repos/medley-test/medley" 2>/dev/null ||
      wget -q -O /dev/null "http://127.0.0.1:${PORT}/repos/medley-test/medley" 2>/dev/null; then
      return 0
    fi
    i=$((i + 1))
    sleep 0.1
  done
  echo "fixture server did not come up on ${PORT}" >&2
  exit 1
}

stop_server() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    # Reap it, so the shell does not print its own "Terminated" notice over
    # the test output.
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  SERVER_PID=''
}

# A PATH mirroring the real one, minus the downloader we want to exclude.
#
# `select_downloader` prefers curl whenever it is present, so on any machine
# that has both, the wget branch never executes — which is how it stayed
# unexercised long enough to matter. Shadowing curl is not enough: `command -v`
# finds a non-executable stub perfectly well.
#
# Everything else is mirrored rather than enumerated. An allowlist looks
# tidier and is wrong in a way that is hard to read: the first version of this
# omitted `gzip`, and GNU `tar -xzf` shells out to it, so the failure surfaced
# as "could not extract" — an installer bug that was not one.
restricted_path() {
  exclude="$1"
  dir="${WORK}/bin-no-${exclude}"
  rm -rf "$dir"
  mkdir -p "$dir"
  saved_ifs="$IFS"
  IFS=:
  for entry in $PATH; do
    IFS="$saved_ifs"
    [ -d "$entry" ] || { IFS=:; continue; }
    for candidate in "$entry"/*; do
      [ -x "$candidate" ] || continue
      base="${candidate##*/}"
      [ "$base" = "$exclude" ] && continue
      [ -e "${dir}/${base}" ] || ln -s "$candidate" "${dir}/${base}" 2>/dev/null || true
    done
    IFS=:
  done
  IFS="$saved_ifs"
  if [ "$exclude" = "curl" ] && ! command -v wget >/dev/null 2>&1; then
    curl_bin=""
    IFS=:
    for entry in $PATH; do
      IFS="$saved_ifs"
      if [ -x "${entry}/curl" ]; then
        curl_bin="${entry}/curl"
        break
      fi
      IFS=:
    done
    IFS="$saved_ifs"
    if [ -n "$curl_bin" ]; then
      cat > "${dir}/wget" <<MOCKWGET
#!/bin/sh
out=""
url=""
server_resp=0
while [ \$# -gt 0 ]; do
  case "\$1" in
    -q|--quiet) shift ;;
    -O|--output-document) out="\$2"; shift 2 ;;
    --output-document=*) out="\${1#--output-document=}"; shift ;;
    -O*) out="\${1#-O}"; shift ;;
    --server-response) server_resp=1; shift ;;
    --tries=*) shift ;;
    *) url="\$1"; shift ;;
  esac
done
if [ "\$server_resp" = 1 ]; then
  status_code="\$( "$curl_bin" --silent --location --output "\$out" --write-out '%{http_code}' "\$url" 2>/dev/null )" || exit 1
  printf '  HTTP/1.1 %s OK\n' "\$status_code" >&2
  if [ "\$status_code" -ge 400 ] || [ "\$status_code" -eq 0 ]; then
    exit 1
  fi
else
  if [ -n "\$out" ]; then
    exec "$curl_bin" --fail --silent --show-error --location -o "\$out" "\$url"
  else
    exec "$curl_bin" --fail --silent --show-error --location "\$url"
  fi
fi
MOCKWGET
      chmod 755 "${dir}/wget"
    fi
  fi
  printf '%s\n' "$dir"
}

# install.sh with only the hosts redirected.
script_under_test() {
  sed -e "s|https://api.github.com|http://127.0.0.1:${PORT}|g" \
    -e "s|https://github.com|http://127.0.0.1:${PORT}|g" \
    "${REPO_ROOT}/install.sh" > "${WORK}/install-under-test.sh"
  printf '%s\n' "${WORK}/install-under-test.sh"
}

run_case() {
  # run_case <name> <scenario> <tamper|clean> <expected-exit> <expect-substring>
  name="$1"; scenario="$2"; tamper="$3"; want_exit="$4"; want_text="$5"
  assets="${WORK}/assets"
  rm -rf "$assets"
  build_fixture_release "$assets" "$tamper"
  start_server "$scenario" "$assets"

  home="${WORK}/home-${name}"
  rm -rf "$home"
  mkdir -p "$home"
  out="${WORK}/out-${name}.txt"

  script="$(script_under_test)"
  set +e
  if [ -n "${EXCLUDE_DOWNLOADER:-}" ]; then
    HOME="$home" MEDLEY_REPO=medley-test/medley \
      PATH="$(restricted_path "$EXCLUDE_DOWNLOADER")" \
      sh "$script" > "$out" 2>&1
  else
    HOME="$home" MEDLEY_REPO=medley-test/medley sh "$script" > "$out" 2>&1
  fi
  got_exit=$?
  set -e
  stop_server

  if [ "$got_exit" = "$want_exit" ]; then
    ok "${name}: exit ${got_exit}"
  else
    bad "${name}: expected exit ${want_exit}, got ${got_exit}"
    sed 's/^/       /' "$out" | tail -5
  fi

  if [ -z "$want_text" ] || grep -q "$want_text" "$out"; then
    ok "${name}: output"
  else
    bad "${name}: expected output to contain '${want_text}'"
    sed 's/^/       /' "$out" | tail -5
  fi

  CASE_HOME="$home"
}

DOWNLOADER_LABEL="${EXCLUDE_DOWNLOADER:+wget-only (curl hidden)}"
echo "== installer cases (${TARGET}) ${DOWNLOADER_LABEL:-curl} =="

# A release that exists installs, and the result is usable.
# This is also the deterministic `/releases/latest` resolution case (#256):
# the fixture answers 200 + tag_name, and the installer must pick that tag
# without a live GitHub quota.
run_case ok ok clean 0 'Checksum verified'
if [ -x "${CASE_HOME}/.medley/bin/medley" ]; then
  ok "ok: the command is executable"
else
  bad "ok: no executable at ~/.medley/bin/medley"
fi
if [ -e "${CASE_HOME}/.grok" ]; then
  bad "ok: the official Grok Build state directory was created"
else
  ok "ok: ~/.grok untouched"
fi

# Re-running must be a no-op, not a failure or a duplicate tree.
start_server ok "${WORK}/assets"
set +e
HOME="$CASE_HOME" MEDLEY_REPO=medley-test/medley sh "${WORK}/install-under-test.sh" > "${WORK}/out-idempotent.txt" 2>&1
second_exit=$?
set -e
stop_server
if [ "$second_exit" = 0 ]; then
  ok "idempotent: a second install succeeds"
else
  bad "idempotent: second install exited ${second_exit}"
  tail -5 "${WORK}/out-idempotent.txt" | sed 's/^/       /'
fi
versions="$(find "${CASE_HOME}/.medley/versions" -maxdepth 1 -mindepth 1 -type d | wc -l | tr -d ' ')"
if [ "$versions" = 1 ]; then
  ok "idempotent: exactly one version tree remains"
else
  bad "idempotent: ${versions} version trees, expected 1"
fi

# A checksum that does not match must abort before anything is activated.
run_case bad-checksum ok tamper 1 'checksum mismatch'
if [ -e "${CASE_HOME}/.medley/bin/medley" ]; then
  bad "bad-checksum: a command was installed anyway"
else
  ok "bad-checksum: nothing was installed"
fi

# Nothing published as latest, but the repository is there.
run_case no-release no-release clean 1 'nothing is published as latest'

# The repository itself does not answer.
run_case no-repo no-repo clean 1 'could not read'

# Issue #83: a final 3xx carrying a tag-shaped body must not become a version.
run_case redirect-body redirect-body clean 1 ''
if grep -q '0\.0\.1' "${WORK}/out-redirect-body.txt"; then
  bad "redirect-body: the tag from a non-2xx response was used"
  sed 's/^/       /' "${WORK}/out-redirect-body.txt" | head -8
else
  ok "redirect-body: the tag from a non-2xx response was rejected"
fi

echo
echo "== ${PASS} passed, ${FAIL} failed =="
[ "$FAIL" = 0 ]
