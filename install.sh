#!/bin/sh
#
# medley installer.
#
# medley is a community distribution of a fork of grok-build. It is not
# affiliated with, sponsored by, or endorsed by xAI.
#
# This script installs a `medley` command that coexists with an official Grok
# Build installation: it never reads or writes ~/.grok, and it never touches an
# existing `grok` binary.
#
#   curl -fsSL https://raw.githubusercontent.com/ImL1s/grok-build/providers/install.sh | sh
#
# Environment:
#   MEDLEY_VERSION      version or tag to install (default: latest release)
#   MEDLEY_INSTALL_DIR  where the `medley` symlink goes (default: ~/.medley/bin)
#   MEDLEY_HOME         where unpacked versions live (default: ~/.medley)
#   MEDLEY_TARGET       force a target triple instead of detecting one
#   MEDLEY_REPO         source repository (default: ImL1s/grok-build)
#   MEDLEY_DRYRUN       set to 1 to print the plan without downloading anything

set -eu

DIST_NAME=medley
REPO="${MEDLEY_REPO:-ImL1s/grok-build}"
MEDLEY_HOME="${MEDLEY_HOME:-${HOME}/.medley}"
INSTALL_DIR="${MEDLEY_INSTALL_DIR:-${MEDLEY_HOME}/bin}"
DRYRUN="${MEDLEY_DRYRUN:-0}"

# The official Grok Build state directory. medley must never write here.
GROK_HOME="${HOME}/.grok"

say() {
  printf '%s\n' "$*"
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# Resolve the archive triple from the running machine.
detect_target() {
  detect_os="$(uname -s)"
  detect_arch="$(uname -m)"

  case "$detect_os" in
  Darwin) detect_os_part='apple-darwin' ;;
  Linux) detect_os_part='unknown-linux-gnu' ;;
  *)
    die "unsupported operating system '${detect_os}'. medley publishes macOS and Linux builds; build from source for anything else."
    ;;
  esac

  case "$detect_arch" in
  arm64 | aarch64) detect_arch_part='aarch64' ;;
  x86_64 | amd64) detect_arch_part='x86_64' ;;
  *)
    die "unsupported architecture '${detect_arch}'. medley publishes x86_64 and aarch64 builds."
    ;;
  esac

  printf '%s-%s\n' "$detect_arch_part" "$detect_os_part"
}

# Pick a downloader once so the rest of the script does not re-probe.
select_downloader() {
  if command -v curl >/dev/null 2>&1; then
    DOWNLOADER=curl
  elif command -v wget >/dev/null 2>&1; then
    DOWNLOADER=wget
  else
    die 'neither curl nor wget is available; install one of them and re-run.'
  fi
}

fetch_to_file() {
  if [ "$DOWNLOADER" = curl ]; then
    curl --fail --silent --show-error --location --retry 3 --output "$2" "$1"
  else
    wget --quiet --tries=3 --output-document="$2" "$1"
  fi
}

fetch_to_stdout() {
  if [ "$DOWNLOADER" = curl ]; then
    curl --fail --silent --show-error --location --retry 3 "$1"
  else
    wget --quiet --tries=3 --output-document=- "$1"
  fi
}

# Same as `fetch_to_stdout`, except an HTTP error status yields its response
# body instead of a failure. Only `resolve_version` wants that: GitHub answers
# 404 when a repository has no release published as latest, and the guidance
# for an empty release channel is nothing like the guidance for an unreachable
# GitHub. Callers separate the two by whether a body came back at all.
fetch_body_allow_http_error() {
  if [ "$DOWNLOADER" = curl ]; then
    curl --silent --show-error --location --retry 3 "$1"
  else
    # --content-on-error keeps the error body; wget still exits non-zero on an
    # error status, so callers must tolerate that exit and inspect the body.
    wget --quiet --tries=3 --content-on-error --output-document=- "$1"
  fi
}

# Fork tags carry build metadata (v1.2.3+providers.4), and GitHub itself links
# to those releases with the '+' percent-encoded. A bare '+' in a URL path is
# not routed reliably, so encode it before fetching.
url_escape() {
  printf '%s\n' "$1" | sed 's/+/%2B/g'
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | sed 's/.*= *//'
  else
    die 'no SHA-256 tool found; install sha256sum, shasum, or openssl.'
  fi
}

# Latest published release. Drafts and prereleases are deliberately excluded:
# the release workflow publishes drafts, so a draft only becomes installable
# once a maintainer publishes it.
resolve_version() {
  if [ -n "${MEDLEY_VERSION:-}" ]; then
    printf '%s\n' "${MEDLEY_VERSION#v}"
    return
  fi

  # An HTTP error must not abort here. GitHub reports "no release published as
  # latest" as a 404, and --fail would collapse that into the same failure as
  # an unreachable network — which is what actually happened the first time
  # this ran against a repository whose only release was a prerelease. A
  # transport failure yields no body, which is what separates the two.
  resolve_body="$(
    fetch_body_allow_http_error \
      "https://api.github.com/repos/${REPO}/releases/latest" || :
  )"

  [ -n "$resolve_body" ] ||
    die "could not reach the GitHub release API for ${REPO}. Set MEDLEY_VERSION to install a specific version."

  resolve_tag="$(
    printf '%s\n' "$resolve_body" |
      sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
      head -n 1
  )"

  if [ -z "$resolve_tag" ]; then
    # Every GitHub API refusal arrives as {"message": ...}, and they are not
    # interchangeable. An unauthenticated client that has exhausted the hourly
    # rate limit gets 403 with a body, which would otherwise fall through the
    # 404 reasoning below and be reported as a missing repository.
    resolve_message="$(
      printf '%s\n' "$resolve_body" |
        sed -n 's/.*"message"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
        head -n 1
    )"

    if [ "$resolve_message" != "Not Found" ]; then
      die "the GitHub release API rejected the request for ${REPO}${resolve_message:+ (${resolve_message})}. Set MEDLEY_VERSION to install a specific tag."
    fi

    # 404 stays ambiguous: "no release published as latest" and "this
    # repository does not exist" carry a byte-identical body. Only the
    # repository endpoint separates them, so ask it before telling someone
    # with a typo in MEDLEY_REPO to go publish a draft release. Error path
    # only — the success path still costs a single request.
    if fetch_to_stdout "https://api.github.com/repos/${REPO}" >/dev/null 2>&1; then
      die "${REPO} has no release published as latest. Drafts and prereleases are both excluded, so publish the draft release the release workflow created — or set MEDLEY_VERSION to install a specific tag."
    fi
    die "cannot read releases for ${REPO}: the repository is unreachable, private, or does not exist. Check MEDLEY_REPO."
  fi

  printf '%s\n' "${resolve_tag#v}"
}

# Collapse '.' and '..' textually. Only ever called on a path whose existing
# prefix has already been resolved with `pwd -P`, so no component being
# collapsed here can be a symlink and the result is the true physical path.
normalize_path() {
  printf '%s\n' "$1" | awk -F/ '
    {
      depth = 0
      for (i = 1; i <= NF; i++) {
        if ($i == "" || $i == ".") continue
        if ($i == "..") { if (depth > 0) depth--; continue }
        stack[++depth] = $i
      }
      out = ""
      for (i = 1; i <= depth; i++) out = out "/" stack[i]
      print (out == "" ? "/" : out)
    }'
}

# One resolution pass: walk up to the nearest existing ancestor, resolve *that*
# through `pwd -P` (which follows symlinks), re-attach the remainder, and
# collapse any '.' or '..' left in it.
canonicalize_once() {
  once_rest=''
  once_head="$1"
  while [ ! -d "$once_head" ]; do
    once_leaf="$(basename "$once_head")"
    once_head="$(dirname "$once_head")"
    if [ -n "$once_rest" ]; then
      once_rest="${once_leaf}/${once_rest}"
    else
      once_rest="$once_leaf"
    fi
    [ "$once_head" != '/' ] || break
  done

  once_head="$(cd "$once_head" 2>/dev/null && pwd -P)" ||
    die "could not resolve ${1}"

  if [ -n "$once_rest" ]; then
    normalize_path "${once_head%/}/${once_rest}"
  else
    normalize_path "$once_head"
  fi
}

# Physical path of something that may not exist yet.
#
# `realpath` would be simpler but is neither POSIX nor present on every macOS.
#
# One pass is not enough. Collapsing '..' can expose a symlink the '..' was
# hiding: 'outer/missing/../grok-link/sub' collapses to 'outer/grok-link/sub'
# with grok-link still unresolved, which is exactly how a path can end up
# inside ~/.grok while looking like it is not. Repeat until the answer stops
# changing — the fixed point is reached once no unresolved symlink remains in
# the existing prefix.
canonicalize() {
  canon_result="$1"
  case "$canon_result" in
  /*) ;;
  *) canon_result="$(pwd -P)/${canon_result}" ;;
  esac

  canon_round=0
  while [ "$canon_round" -lt 40 ]; do
    canon_previous="$canon_result"
    canon_result="$(canonicalize_once "$canon_result")"
    if [ "$canon_result" = "$canon_previous" ]; then
      printf '%s\n' "$canon_result"
      return 0
    fi
    canon_round=$((canon_round + 1))
  done

  die "could not resolve ${1}: too many levels of symbolic links."
}

# `version` and `target` are interpolated into install paths, archive names and
# release URLs. Anything but a plain path component could climb out of the
# install root or point the download somewhere else entirely.
assert_path_component() {
  case "$2" in
  '' | . | ..)
    die "${1} must not be empty, '.', or '..' (got '${2}')"
    ;;
  */* | *\\*)
    die "${1} must be a single path component (got '${2}')"
    ;;
  -*)
    die "${1} must not start with '-' (got '${2}')"
    ;;
  *[!A-Za-z0-9._+-]*)
    die "${1} may only contain letters, digits, '.', '_', '+' and '-' (got '${2}')"
    ;;
  esac
}

# Installing into ~/.grok would put fork state where the official CLI reads it,
# which is the exact corruption this fork exists to avoid. Comparing the paths
# as written is not enough: '$HOME/.medley/../.grok', or a MEDLEY_HOME symlink
# pointing inside ~/.grok, both read as outside it. Resolve first, compare
# after, and keep the resolved paths so the rest of the script writes exactly
# what was checked.
assert_outside_grok_home() {
  guard_grok="$(canonicalize "$GROK_HOME")"
  MEDLEY_HOME="$(canonicalize "$MEDLEY_HOME")"
  INSTALL_DIR="$(canonicalize "$INSTALL_DIR")"

  for guard_path in "$MEDLEY_HOME" "$INSTALL_DIR"; do
    case "$guard_path" in
    "$guard_grok" | "$guard_grok"/*)
      die "refusing to install into ${guard_path}: that resolves inside ${guard_grok}, which belongs to the official Grok Build and medley must not share it."
      ;;
    esac
  done

  # The launcher in INSTALL_DIR execs the payload under versions/. If
  # INSTALL_DIR resolved inside versions/ the launcher would overwrite the
  # binary it points at and exec itself forever.
  guard_versions="${MEDLEY_HOME}/versions"
  case "$INSTALL_DIR" in
  "$guard_versions" | "$guard_versions"/*)
    die "refusing to install the launcher into ${INSTALL_DIR}: that is inside ${guard_versions}, where the versioned payloads live. Choose an install dir outside versions/."
    ;;
  esac
}

report_coexistence() {
  if command -v grok >/dev/null 2>&1; then
    grok_path="$(command -v grok)"
    warn "an existing 'grok' command is installed at ${grok_path}."
    warn "medley installs alongside it as a separate 'medley' command with its own state in ${MEDLEY_HOME}; ~/.grok and the grok binary are left untouched."
  fi
}

print_path_guidance() {
  case ":${PATH}:" in
  *":${INSTALL_DIR}:"*)
    say "${INSTALL_DIR} is already on your PATH."
    ;;
  *)
    say ''
    say "Add ${INSTALL_DIR} to your PATH:"
    say ''
    say "    export PATH=\"${INSTALL_DIR}:\$PATH\""
    say ''
    say 'Then add that line to ~/.zshrc, ~/.bashrc, or your shell'"'"'s equivalent to make it permanent.'
    ;;
  esac
}

# Undo a partial install. `previous` is only non-empty between moving the old
# version aside and finishing activation, so an interruption inside that window
# puts the working version back.
#
# Safe to run twice: a signal runs it, and the EXIT trap may run it again as
# the shell dies. Every step is already conditional on the state it undoes.
# Single-quote a value so it can be embedded verbatim in the generated launcher.
shell_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# Write the `medley` command: a launcher that pins the state directory before
# handing off to the real binary.
#
# Installing the binary alone is not enough. Its config layer falls back to
# ~/.grok when nothing overrides it, so a bare `medley` would read and write the
# official Grok Build's state — precisely the collision this fork exists to
# avoid. That layer honours GROK_HOME today and will prefer MEDLEY_HOME once the
# runtime state-dir work lands, so both are pointed at the same directory and
# this launcher stays correct before and after.
#
# A caller's own choice always wins: an explicit GROK_HOME is left alone
# (setting MEDLEY_HOME would silently outrank it later), while an explicit
# MEDLEY_HOME is mirrored into GROK_HOME so today's builds honour it too.
write_launcher() {
  cat > "$1" <<LAUNCHER
#!/bin/sh
# Generated by the medley installer. Do not edit; reinstalling overwrites it.
if [ -n "\${MEDLEY_HOME:-}" ]; then
  GROK_HOME="\${GROK_HOME:-\$MEDLEY_HOME}"
  export GROK_HOME
elif [ -z "\${GROK_HOME:-}" ]; then
  MEDLEY_HOME=$(shell_quote "$MEDLEY_HOME")
  GROK_HOME="\$MEDLEY_HOME"
  export MEDLEY_HOME GROK_HOME
fi
exec $(shell_quote "$2") "\$@"
LAUNCHER
}

cleanup() {
  [ -z "$tmp" ] || rm -rf "$tmp"
  [ -z "$staging" ] || rm -rf "$staging"
  [ -z "$link_tmp" ] || rm -f "$link_tmp"
  # Already-committed leftovers: scratch to discard, never to restore from.
  [ -z "$doomed" ] || rm -rf "$doomed"
  if [ -n "$previous" ] && [ -d "$previous" ]; then
    rm -rf "$version_dir"
    mv "$previous" "$version_dir"
    warn "install did not finish; restored the previous version at ${version_dir}"
  fi
  :
}

# Clean up, then die from the signal rather than returning.
#
# `trap cleanup INT` alone would run cleanup and let the shell carry straight
# on: an interrupted install would roll back, resume, and still exit 0 claiming
# success. Clearing the trap and re-signalling ourselves gives the caller the
# conventional 128+N status and a genuine signal death.
on_signal() {
  cleanup
  trap - "$1"
  kill -s "$1" "$$"
}

main() {
  select_downloader
  assert_outside_grok_home

  if [ -n "${MEDLEY_TARGET:-}" ]; then
    target="$MEDLEY_TARGET"
  else
    target="$(detect_target)"
  fi
  assert_path_component 'target' "$target"

  version="$(resolve_version)"
  assert_path_component 'version' "$version"

  tag="v${version}"
  stage="${DIST_NAME}-${version}-${target}"
  archive="${stage}.tar.gz"
  checksums="${DIST_NAME}-${version}-checksums.txt"
  base_url="https://github.com/${REPO}/releases/download/$(url_escape "$tag")"
  archive_url="${base_url}/$(url_escape "$archive")"
  checksums_url="${base_url}/$(url_escape "$checksums")"
  versions_root="${MEDLEY_HOME}/versions"
  version_dir="${versions_root}/${version}"

  say "medley ${version} (${target})"
  say "  archive:     ${archive_url}"
  say "  checksums:   ${checksums_url}"
  say "  unpack to:   ${version_dir}"
  say "  symlink:     ${INSTALL_DIR}/${DIST_NAME}"

  if [ "$DRYRUN" = 1 ]; then
    say ''
    say 'MEDLEY_DRYRUN=1 — nothing was downloaded, extracted, or linked.'
    return 0
  fi

  report_coexistence

  # Initialised before the trap so cleanup can never touch an unset variable.
  tmp=''
  staging=''
  previous=''
  doomed=''
  link_tmp=''
  trap cleanup EXIT
  trap 'on_signal INT' INT
  trap 'on_signal TERM' TERM
  trap 'on_signal HUP' HUP
  tmp="$(mktemp -d)"

  say ''
  say "Downloading ${archive}..."
  fetch_to_file "$archive_url" "${tmp}/${archive}" ||
    die "could not download ${archive_url}. Check that release ${tag} exists and publishes a build for ${target}."

  fetch_to_file "$checksums_url" "${tmp}/${checksums}" ||
    die "could not download ${checksums_url}. medley will not install an unverified binary."

  expected="$(awk -v name="$archive" '$2 == name { print $1; exit }' "${tmp}/${checksums}")"
  [ -n "$expected" ] ||
    die "${checksums} has no entry for ${archive}; refusing to install."

  actual="$(sha256_of "${tmp}/${archive}")"
  if [ "$actual" != "$expected" ]; then
    die "checksum mismatch for ${archive}: expected ${expected}, got ${actual}. Do not use this download."
  fi
  say "Checksum verified: ${actual}"

  tar -xzf "${tmp}/${archive}" -C "$tmp" ||
    die "could not extract ${archive}."
  [ -f "${tmp}/${stage}/${DIST_NAME}" ] ||
    die "${archive} does not contain a ${DIST_NAME} binary."

  # Stage inside the versions directory, so that activating is a rename on one
  # filesystem rather than a copy that can be interrupted halfway.
  mkdir -p "$versions_root"
  staging="${versions_root}/.staging-$$"
  rm -rf "$staging"
  mv "${tmp}/${stage}" "$staging"
  chmod +x "${staging}/${DIST_NAME}"

  # Prove the new binary runs before it replaces one that already does.
  smoke="$("${staging}/${DIST_NAME}" --version 2>/dev/null)" ||
    die "the downloaded ${DIST_NAME} could not run on this machine, so your existing installation was left untouched. On macOS this is usually Gatekeeper quarantine."

  # Activate. Both steps are renames, so an interruption leaves either the old
  # version or the new one — never a half-written tree or a dangling symlink.
  if [ -d "$version_dir" ]; then
    previous="${versions_root}/.previous-$$"
    rm -rf "$previous"
    mv "$version_dir" "$previous"
  fi
  mv "$staging" "$version_dir"
  staging=''

  mkdir -p "$INSTALL_DIR"
  link_tmp="${INSTALL_DIR}/.${DIST_NAME}.new-$$"
  rm -f "$link_tmp"
  write_launcher "$link_tmp" "${version_dir}/${DIST_NAME}"
  chmod 755 "$link_tmp"
  mv -f "$link_tmp" "${INSTALL_DIR}/${DIST_NAME}"
  link_tmp=''

  # The command now points at the new version, so the install is committed.
  # Disarm the rollback *before* touching the old tree: an interrupt during the
  # delete below must leave the new version in place, not restore a half-erased
  # old one over it.
  doomed="$previous"
  previous=''
  [ -z "$doomed" ] || rm -rf "$doomed"
  doomed=''

  say "Installed ${DIST_NAME} ${smoke} to ${INSTALL_DIR}/${DIST_NAME}"

  print_path_guidance
  say ''
  say "State lives in ${MEDLEY_HOME}. medley does not read or write ~/.grok."
}

main "$@"
