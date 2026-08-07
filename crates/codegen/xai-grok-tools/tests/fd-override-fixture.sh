#!/bin/sh
set -eu

if [ "${1-}" = "--version" ]; then
  echo "fd override fixture 1.0"
  exit 0
fi

echo "fd override fixture"
