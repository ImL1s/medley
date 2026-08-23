#!/bin/sh
#
# Run a published musl archive on a distribution the gnu archives cannot reach.
#
#   sh musl_smoke.sh <archive.tar.gz> <target-triple> <version>
#
# Runs *inside* rockylinux:9 or amazonlinux:2023 (glibc 2.34, below the 2.35
# floor the gnu archives promise). Everything it needs from outside the
# container is mounted at /smoke: this script, the DNS+TLS harness, and a
# throwaway CA. Nothing is installed into the image — the point is what the
# static binary can do on a stock system, so adding packages to make it work
# would answer a different question than the one asked (issue #82).
#
# What is actually proven, and why each one is here:
#
#   1. the binary starts at all             — the failure this issue opens with
#                                             is a checksum-verified archive
#                                             that dies on a missing symbol
#   2. it reports the triple it was built   — `buildTarget` (#100), so a gnu
#      for                                    binary inside a musl-named
#                                             archive cannot pass
#   3. it can exec another program          — musl's process layer, from a
#                                             statically linked caller
#   4. it can resolve a name and complete   — the load-bearing one: a static
#      a TLS handshake                        binary loads no NSS modules, so
#                                             musl's own resolver is the only
#                                             thing answering
#
# (3) is `execve`, not `fork` — `wrap` replaces its own process on Unix. It
# proves the binary can hand off to another program, which is the part a
# static build could plausibly break; it is not a claim about `fork`.
#
# (4) asserts on what the harness observed, never on medley's wording. A line
# in the TLS log can only exist if the name resolved *and* the handshake
# completed, because the harness cannot see a request that got neither.
set -eu

ARCHIVE="${1:?usage: musl_smoke.sh <archive> <target> <version>}"
TARGET="${2:?usage: musl_smoke.sh <archive> <target> <version>}"
VERSION="${3:?usage: musl_smoke.sh <archive> <target> <version>}"

SMOKE_DIR=/smoke
WORK=/tmp/medley-musl-smoke
RESOLVES=medley-smoke.test
# Deliberately never answered, to get a negative control out of the same
# harness: the DNS log must gain this name and the TLS log must not grow.
NXDOMAIN=medley-smoke-absent.test
HTTPS_PORT=8443
MARKER=medley-exec-marker-8a41f2

DNS_LOG="${WORK}/dns.log"
TLS_LOG="${WORK}/tls.log"
READY="${WORK}/harness.ready"

PASS=0
FAIL=0

ok() {
  PASS=$((PASS + 1))
  printf '  ok   %s\n' "$1"
}

bad() {
  FAIL=$((FAIL + 1))
  printf '  FAIL %s\n' "$1"
}

rm -rf "$WORK"
mkdir -p "$WORK"

DISTRO="$(sed -n 's/^PRETTY_NAME="\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' /etc/os-release 2>/dev/null | head -n 1)"
echo "== ${TARGET} on ${DISTRO:-unknown} =="
echo "   glibc here: $(getconf GNU_LIBC_VERSION 2>/dev/null || echo 'none')"

# The harness needs an interpreter, and both images ship one because dnf is
# written in Python. Assert it rather than discovering it as a mystery failure
# three steps later.
if ! python3 --version; then
  echo "error: no python3 in this image; the DNS and TLS legs cannot run" >&2
  exit 1
fi

# ---------------------------------------------------------------- the binary

tar -xzf "$ARCHIVE" -C "$WORK"
BIN="${WORK}/medley-${VERSION}-${TARGET}/medley"
if [ ! -x "$BIN" ]; then
  echo "error: ${ARCHIVE} did not unpack an executable at ${BIN}" >&2
  ls -R "$WORK" >&2
  exit 1
fi

# glibc's loader is what would refuse a gnu binary here, so this line failing
# is the exact user-visible symptom issue #82 describes.
if version_output="$("$BIN" --version 2>&1)"; then
  ok "starts: ${version_output}"
else
  bad "the binary does not run on this distribution: ${version_output}"
  echo "== ${PASS} passed, ${FAIL} failed =="
  exit 1
fi

json_output="$("$BIN" version --json)"
case "$json_output" in
*"\"buildTarget\":\"${TARGET}\""*)
  ok "reports buildTarget ${TARGET}"
  ;;
*)
  bad "buildTarget is not ${TARGET}: ${json_output}"
  ;;
esac

# --------------------------------------------------------------------- exec

exec_output="$("$BIN" wrap /bin/echo "$MARKER" 2>&1 || true)"
case "$exec_output" in
*"$MARKER"*)
  ok "execs a child program"
  ;;
*)
  bad "wrap did not reach /bin/echo: ${exec_output}"
  ;;
esac

# ----------------------------------------------------------- DNS and TLS

python3 "${SMOKE_DIR}/musl_smoke_server.py" \
  --host "$RESOLVES" \
  --https-port "$HTTPS_PORT" \
  --cert "${SMOKE_DIR}/server.pem" \
  --key "${SMOKE_DIR}/server.key" \
  --dns-log "$DNS_LOG" \
  --tls-log "$TLS_LOG" \
  --ready-file "$READY" &
HARNESS_PID=$!

cleanup() {
  kill "$HARNESS_PID" 2>/dev/null || true
  wait "$HARNESS_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Wait for both sockets rather than sleeping a guess.
waited=0
while [ ! -f "$READY" ]; do
  waited=$((waited + 1))
  if [ "$waited" -gt 100 ]; then
    echo "error: the harness did not bind its sockets" >&2
    exit 1
  fi
  sleep 0.1
done
ok "harness listening on dns/53 and https/${HTTPS_PORT}"

# Point the container's resolver at the harness. musl reads this file on every
# lookup, so no cache has to be invalidated.
printf 'nameserver 127.0.0.1\n' > /etc/resolv.conf

# The logs start empty, which is what makes their contents afterwards evidence
# rather than something that could have been there all along.
if [ -s "$TLS_LOG" ]; then
  bad "the TLS log was not empty before any medley run"
else
  ok "TLS log empty before the run"
fi

# `models` is the lightest command that talks to the configured endpoint. Both
# base URLs are overridden the same way this repository's own test support
# overrides them. The key is a fixed placeholder: the harness answers 401 to
# everything, and the assertion is on what the harness saw, not on the reply.
run_medley() {
  HOME="${WORK}/home" \
    MEDLEY_HOME="${WORK}/home/.medley" \
    GROK_HOME="${WORK}/home/.medley" \
    XAI_API_KEY=medley-musl-smoke-placeholder \
    GROK_EXTRA_CA_BUNDLE="${SMOKE_DIR}/ca.pem" \
    GROK_XAI_API_BASE_URL="https://${1}:${HTTPS_PORT}/v1" \
    GROK_MODELS_BASE_URL="https://${1}:${HTTPS_PORT}/v1" \
    timeout 120 "$BIN" models > "${WORK}/models-${2}.log" 2>&1 || true
}

mkdir -p "${WORK}/home"
run_medley "$RESOLVES" resolves

if grep -q "^${RESOLVES} " "$DNS_LOG"; then
  ok "resolved ${RESOLVES} through musl's resolver"
else
  bad "no DNS query for ${RESOLVES} reached the harness"
  sed 's/^/       /' "${WORK}/models-resolves.log" | tail -15
fi

if [ -s "$TLS_LOG" ]; then
  ok "completed a TLS handshake and sent a request: $(head -n 1 "$TLS_LOG")"
else
  bad "no request survived the TLS handshake"
  sed 's/^/       /' "${WORK}/models-resolves.log" | tail -15
fi

# Negative control. The same harness, the same binary, a name it will not
# answer: the resolver must be asked and the TLS server must not be reached.
# Without this, a TLS log that was somehow pre-populated would read as a pass.
tls_before="$(wc -l < "$TLS_LOG" | tr -d ' ')"
run_medley "$NXDOMAIN" nxdomain

if grep -q "^${NXDOMAIN} " "$DNS_LOG"; then
  ok "control: ${NXDOMAIN} was queried"
else
  bad "control: ${NXDOMAIN} never reached the resolver, so the check above proves nothing about DNS"
fi

tls_after="$(wc -l < "$TLS_LOG" | tr -d ' ')"
if [ "$tls_after" = "$tls_before" ]; then
  ok "control: an unresolvable name reached no TLS request"
else
  bad "control: the TLS log grew from ${tls_before} to ${tls_after} for a name that does not resolve"
fi

echo
echo "== ${PASS} passed, ${FAIL} failed =="
[ "$FAIL" = 0 ]
