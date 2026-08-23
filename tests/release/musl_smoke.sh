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
#   4. it can resolve a name and open a      — the load-bearing one: a static
#      TLS connection                          binary loads no NSS modules, so
#                                              musl's own resolver is the only
#                                              thing answering
#
# (3) is `execve`, not `fork` — `wrap` replaces its own process on Unix. It
# proves the binary can hand off to another program, which is the part a
# static build could plausibly break; it is not a claim about `fork`.
#
# (4) asserts on what the harness observed, never on medley's wording. The
# required observation is a TLS ClientHello, which the harness cannot see
# unless the name resolved, TCP connected, and the client's TLS stack produced
# a handshake message. A completed request is reported too when it happens,
# but is not required: that additionally needs whichever subsystem made the
# call to have wired GROK_EXTRA_CA_BUNDLE, which is a property of this
# binary's plumbing rather than of musl.
#
# The driver is `setup`, which fetches managed configuration from
# `[endpoints] managed_config_url` once a deployment key is present. It was
# chosen by measurement, not by reading: `models` looks like the obvious
# candidate and makes *no* network request at all — it answers from a local
# catalogue, so a smoke test built on it would have asserted nothing and
# failed at the first tag.
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
ok "harness listening on dns/53 and tls/${HTTPS_PORT}"

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

# `setup` fetches managed configuration over HTTPS as soon as a deployment key
# is present, from whatever `[endpoints] managed_config_url` names — so the
# whole request is steered by config this script writes, with no credential
# that has to be real. The key is a fixed placeholder; the harness answers
# every request the same way, and the assertions are on what it observed.
run_medley() {
  rm_home="${WORK}/home-${2}"
  rm -rf "$rm_home"
  mkdir -p "${rm_home}/.medley"
  printf '[endpoints]\nmanaged_config_url = "https://%s:%s/deployment/config"\n' \
    "$1" "$HTTPS_PORT" > "${rm_home}/.medley/config.toml"

  HOME="$rm_home" \
    MEDLEY_HOME="${rm_home}/.medley" \
    GROK_HOME="${rm_home}/.medley" \
    GROK_DEPLOYMENT_KEY=medley-musl-smoke-placeholder \
    GROK_EXTRA_CA_BUNDLE="${SMOKE_DIR}/ca.pem" \
    timeout 120 "$BIN" setup --json > "${WORK}/setup-${2}.log" 2>&1 || true
}

run_medley "$RESOLVES" resolves

if grep -q "^${RESOLVES} " "$DNS_LOG"; then
  ok "resolved ${RESOLVES} through musl's resolver"
else
  bad "no DNS query for ${RESOLVES} reached the harness"
  sed 's/^/       /' "${WORK}/setup-resolves.log" | tail -15
fi

# The required signal. A ClientHello means the name resolved, TCP connected,
# and rustls came up far enough to speak — which is everything musl is
# responsible for here.
if grep -q '^CLIENTHELLO ' "$TLS_LOG"; then
  ok "opened a TLS connection: $(grep -m 1 '^CLIENTHELLO ' "$TLS_LOG")"
else
  bad "no TLS ClientHello reached the harness"
  echo "       tls log:"
  sed 's/^/       /' "$TLS_LOG"
  sed 's/^/       /' "${WORK}/setup-resolves.log" | tail -15
fi

# Reported, not required — see the header. When it does appear, certificate
# verification worked too, which is worth knowing but is not musl's doing.
if grep -q '^REQUEST ' "$TLS_LOG"; then
  ok "handshake completed and a request arrived: $(grep -m 1 '^REQUEST ' "$TLS_LOG")"
else
  printf '  info %s\n' "handshake did not complete; the caller did not trust the throwaway CA ($(grep -m 1 '^HANDSHAKE-FAILED ' "$TLS_LOG" || echo 'no handshake error logged'))"
fi

# Negative control. The same harness, the same binary, a name it will not
# answer: the resolver must be asked and no connection may arrive. Without
# this, a TLS log that was somehow pre-populated would read as a pass.
tls_before="$(grep -c '^CLIENTHELLO ' "$TLS_LOG" || true)"
run_medley "$NXDOMAIN" nxdomain

if grep -q "^${NXDOMAIN} " "$DNS_LOG"; then
  ok "control: ${NXDOMAIN} was queried"
else
  bad "control: ${NXDOMAIN} never reached the resolver, so the check above proves nothing about DNS"
fi

tls_after="$(grep -c '^CLIENTHELLO ' "$TLS_LOG" || true)"
if [ "$tls_after" = "$tls_before" ]; then
  ok "control: an unresolvable name opened no TLS connection"
else
  bad "control: ClientHellos grew from ${tls_before} to ${tls_after} for a name that does not resolve"
fi

echo
echo "== ${PASS} passed, ${FAIL} failed =="
[ "$FAIL" = 0 ]
