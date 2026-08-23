#!/bin/sh
# Exercise install.sh's Linux libc selection, from any host.
#
# Run from the repository root:  sh tests/installer/target_selection_cases.sh
#
# The choice between the gnu and musl archives is made from the host's glibc
# version, so the interesting hosts are ones no CI runner is: below the floor,
# or with no glibc at all. `tests/installer/run_cases.sh` covers the download
# and verification path on the machine it runs on; this covers the decision
# that happens before any of it, by stubbing what the probes report.
#
# `main` is disarmed out of a copy so the functions can be sourced without the
# script installing anything — the same copy-and-rewrite approach run_cases.sh
# uses to redirect the GitHub hosts.
#
# The stubs are the control. `uname` reports Linux; on a macOS host, a stub
# that failed to take effect would produce `apple-darwin` and every case below
# would fail loudly rather than quietly testing nothing.
set -eu

REPO_ROOT="$(pwd)"
REAL_PATH="$PATH"
WORK="$(mktemp -d)"
PASS=0
FAIL=0

cleanup() {
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

FUNCTIONS="${WORK}/install-functions.sh"
sed 's/^main "$@"$/:/' "${REPO_ROOT}/install.sh" > "$FUNCTIONS"
if grep -q '^main "\$@"$' "$FUNCTIONS"; then
  echo 'error: main() is still armed in the copy; sourcing it would run an install' >&2
  exit 1
fi

# A PATH front-end where the three things detect_target consults answer as told.
#
# An empty <getconf-line> or <ldd-line> makes that probe exit non-zero with no
# output, which is how a host without usable glibc tooling behaves.
make_stubs() {
  # make_stubs <name> <arch> <getconf-line> <ldd-line>
  ms_dir="${WORK}/stub-$1"
  ms_arch="$2"
  ms_getconf="$3"
  ms_ldd="$4"
  rm -rf "$ms_dir"
  mkdir -p "$ms_dir"

  cat > "${ms_dir}/uname" <<UNAME
#!/bin/sh
case "\$1" in
-s) echo Linux ;;
-m) echo '${ms_arch}' ;;
*) echo Linux ;;
esac
UNAME

  cat > "${ms_dir}/getconf" <<GETCONF
#!/bin/sh
[ "\$1" = GNU_LIBC_VERSION ] || exit 1
[ -n '${ms_getconf}' ] || exit 1
echo '${ms_getconf}'
GETCONF

  cat > "${ms_dir}/ldd" <<LDD
#!/bin/sh
case "\$1" in
--version) ;;
*) exit 1 ;;
esac
[ -n '${ms_ldd}' ] || exit 1
echo '${ms_ldd}'
LDD

  chmod 755 "${ms_dir}/uname" "${ms_dir}/getconf" "${ms_dir}/ldd"
  printf '%s\n' "$ms_dir"
}

run_case() {
  # run_case <name> <stub-dir> <MEDLEY_LIBC> <expected-exit> <expected-target>
  rc_name="$1"
  rc_dir="$2"
  rc_libc="$3"
  rc_exit="$4"
  rc_want="$5"

  set +e
  rc_got="$(
    PATH="${rc_dir}:${REAL_PATH}" MEDLEY_LIBC="$rc_libc" \
      sh -c '. "$1"; detect_target' _ "$FUNCTIONS" 2> "${WORK}/err-${rc_name}.txt"
  )"
  rc_status=$?
  set -e

  if [ "$rc_status" = "$rc_exit" ]; then
    ok "${rc_name}: exit ${rc_status}"
  else
    bad "${rc_name}: expected exit ${rc_exit}, got ${rc_status}"
    sed 's/^/       /' "${WORK}/err-${rc_name}.txt"
  fi

  if [ "$rc_got" = "$rc_want" ]; then
    ok "${rc_name}: ${rc_got:-<no target>}"
  else
    bad "${rc_name}: expected '${rc_want}', got '${rc_got}'"
    sed 's/^/       /' "${WORK}/err-${rc_name}.txt"
  fi
}

echo '== install.sh target selection =='

# A current distribution: at or above the floor, so the dynamically linked
# archive is the right one and nothing changes for the majority of users.
MODERN="$(make_stubs modern x86_64 'glibc 2.39' 'ldd (Ubuntu GLIBC 2.39-0ubuntu8.3) 2.39')"
run_case modern-glibc "$MODERN" '' 0 x86_64-unknown-linux-gnu

# Exactly the floor still counts as meeting it.
AT_FLOOR="$(make_stubs at-floor aarch64 'glibc 2.35' 'ldd (Ubuntu GLIBC 2.35-0ubuntu3) 2.35')"
run_case at-the-floor "$AT_FLOOR" '' 0 aarch64-unknown-linux-gnu

# RHEL/Rocky 9 and Amazon Linux 2023. 2.34 is below 2.35 by a hair, and a
# lexical comparison would get 2.9-versus-2.35 style pairs backwards, so this
# is the case the numeric comparison exists for.
BELOW="$(make_stubs below x86_64 'glibc 2.34' 'ldd (GNU libc) 2.34')"
run_case below-floor "$BELOW" '' 0 x86_64-unknown-linux-musl

# Debian 11 on x86_64. Older still, and the one a container-built glibc
# artifact would not have recovered.
OLD="$(make_stubs old x86_64 'glibc 2.31' 'ldd (Debian GLIBC 2.31-13+deb11u11) 2.31')"
run_case debian-11 "$OLD" '' 0 x86_64-unknown-linux-musl

# Alpine: getconf reports nothing usable and ldd names musl. This is the host
# that gets the issue's worst failure today — checksum passes, installer says
# success, binary does not start.
ALPINE="$(make_stubs alpine x86_64 '' 'musl libc (x86_64) Version 1.2.5')"
run_case alpine "$ALPINE" '' 0 x86_64-unknown-linux-musl

# Neither probe answers. Choosing gnu here would trade a working install for a
# broken one on a host that cannot be asked, so the static build wins.
UNKNOWN="$(make_stubs unknown x86_64 '' '')"
run_case no-libc-probe "$UNKNOWN" '' 0 x86_64-unknown-linux-musl

# ---------------------------------------------------------------------------
# aarch64 has no static build to fall back to (#424): upstream ripgrep, which
# the binary embeds, publishes no aarch64 musl asset. A host that would have
# been sent to musl must be told, before anything is downloaded, rather than
# 404ing on an archive that does not exist or installing a gnu one that cannot
# start. These are the cases that would silently regress if the arm64 lane were
# ever half-added.
ARM_BELOW="$(make_stubs arm-below aarch64 'glibc 2.34' 'ldd (GNU libc) 2.34')"
run_case arm64-below-floor "$ARM_BELOW" '' 1 ''
if grep -q 'no static (musl) build for aarch64' "${WORK}/err-arm64-below-floor.txt"; then
  ok 'arm64-below-floor: refuses with a reason instead of downloading'
else
  bad 'arm64-below-floor: no usable message'
  sed 's/^/       /' "${WORK}/err-arm64-below-floor.txt"
fi
if grep -q 'MEDLEY_LIBC=gnu' "${WORK}/err-arm64-below-floor.txt"; then
  ok 'arm64-below-floor: offers the override as a way through'
else
  bad 'arm64-below-floor: dead end with no escape hatch'
fi

ARM_ALPINE="$(make_stubs arm-alpine aarch64 '' 'musl libc (aarch64) Version 1.2.5')"
run_case arm64-alpine "$ARM_ALPINE" '' 1 ''

# Asking for it explicitly must fail the same way, not 404 later.
run_case arm64-override-musl "$ARM_BELOW" musl 1 ''
if grep -q 'MEDLEY_LIBC=musl asked for one' "${WORK}/err-arm64-override-musl.txt"; then
  ok 'arm64-override-musl: names the request as the reason'
else
  bad 'arm64-override-musl: no usable message'
  sed 's/^/       /' "${WORK}/err-arm64-override-musl.txt"
fi

# The override still works in the direction that has an archive.
run_case arm64-override-gnu "$ARM_ALPINE" gnu 0 aarch64-unknown-linux-gnu

# An arm64 host at or above the floor is unaffected by any of this.
ARM_MODERN="$(make_stubs arm-modern aarch64 'glibc 2.39' 'ldd (Ubuntu GLIBC 2.39-0ubuntu8.3) 2.39')"
run_case arm64-modern "$ARM_MODERN" '' 0 aarch64-unknown-linux-gnu

# The override wins in both directions, including against the detection that
# would otherwise have chosen the other one.
run_case override-musl-on-modern "$MODERN" musl 0 x86_64-unknown-linux-musl
run_case override-gnu-on-alpine "$ALPINE" gnu 0 x86_64-unknown-linux-gnu

# An explicit 'auto' is the documented default spelled out.
run_case override-auto "$BELOW" auto 0 x86_64-unknown-linux-musl

# A typo must stop the install rather than silently picking a flavour. Without
# this, `MEDLEY_LIBC=gnu-static` would install musl and never say so.
run_case override-rejected "$MODERN" gnu-static 1 ''
if grep -q "MEDLEY_LIBC must be" "${WORK}/err-override-rejected.txt"; then
  ok 'override-rejected: explains what the valid values are'
else
  bad 'override-rejected: no usable message'
  sed 's/^/       /' "${WORK}/err-override-rejected.txt"
fi

# Choosing musl automatically is a surprise worth explaining, so the reason
# reaches the user rather than only the triple in the download line.
if grep -q 'below the 2.35' "${WORK}/err-below-floor.txt"; then
  ok 'below-floor: says why it switched'
else
  bad 'below-floor: switched silently'
  sed 's/^/       /' "${WORK}/err-below-floor.txt"
fi
if grep -q 'no glibc found' "${WORK}/err-alpine.txt"; then
  ok 'alpine: says why it switched'
else
  bad 'alpine: switched silently'
  sed 's/^/       /' "${WORK}/err-alpine.txt"
fi

# macOS must be untouched by any of this: the libc question does not apply,
# and MEDLEY_LIBC must not leak into a Darwin triple.
DARWIN="${WORK}/stub-darwin"
mkdir -p "$DARWIN"
cat > "${DARWIN}/uname" <<'DARWINUNAME'
#!/bin/sh
case "$1" in
-s) echo Darwin ;;
-m) echo arm64 ;;
*) echo Darwin ;;
esac
DARWINUNAME
chmod 755 "${DARWIN}/uname"
run_case darwin "$DARWIN" musl 0 aarch64-apple-darwin

echo
echo "== ${PASS} passed, ${FAIL} failed =="
[ "$FAIL" = 0 ]
