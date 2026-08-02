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

  resolve_body="$(fetch_to_stdout "https://api.github.com/repos/${REPO}/releases/latest")" ||
    die "could not reach the GitHub release API for ${REPO}. Set MEDLEY_VERSION to install a specific version."

  resolve_tag="$(
    printf '%s\n' "$resolve_body" |
      sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
      head -n 1
  )"

  [ -n "$resolve_tag" ] ||
    die "${REPO} has no published release yet. Publish the draft release, or set MEDLEY_VERSION."

  printf '%s\n' "${resolve_tag#v}"
}

# Installing into ~/.grok would put fork state where the official CLI reads it,
# which is the exact corruption this fork exists to avoid.
assert_outside_grok_home() {
  for guard_path in "$MEDLEY_HOME" "$INSTALL_DIR"; do
    case "$guard_path" in
    "$GROK_HOME" | "$GROK_HOME"/*)
      die "refusing to install into ${guard_path}: ~/.grok belongs to the official Grok Build and medley must not share it."
      ;;
    esac
  done
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

main() {
  select_downloader
  assert_outside_grok_home

  if [ -n "${MEDLEY_TARGET:-}" ]; then
    target="$MEDLEY_TARGET"
  else
    target="$(detect_target)"
  fi

  version="$(resolve_version)"
  tag="v${version}"
  stage="${DIST_NAME}-${version}-${target}"
  archive="${stage}.tar.gz"
  checksums="${DIST_NAME}-${version}-checksums.txt"
  base_url="https://github.com/${REPO}/releases/download/$(url_escape "$tag")"
  archive_url="${base_url}/$(url_escape "$archive")"
  checksums_url="${base_url}/$(url_escape "$checksums")"
  version_dir="${MEDLEY_HOME}/versions/${version}"

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

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT INT TERM

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

  mkdir -p "${MEDLEY_HOME}/versions"
  rm -rf "$version_dir"
  mv "${tmp}/${stage}" "$version_dir"
  chmod +x "${version_dir}/${DIST_NAME}"

  # `ln -n` is not POSIX, so replace the old link rather than relying on it.
  mkdir -p "$INSTALL_DIR"
  rm -f "${INSTALL_DIR}/${DIST_NAME}"
  ln -s "${version_dir}/${DIST_NAME}" "${INSTALL_DIR}/${DIST_NAME}"

  say "Installed ${DIST_NAME} to ${INSTALL_DIR}/${DIST_NAME}"

  if ! "${INSTALL_DIR}/${DIST_NAME}" --version; then
    warn "${DIST_NAME} was installed but did not run. On macOS, try: xattr -d com.apple.quarantine ${version_dir}/${DIST_NAME}"
  fi

  print_path_guidance
  say ''
  say "State lives in ${MEDLEY_HOME}. medley does not read or write ~/.grok."
}

main "$@"
