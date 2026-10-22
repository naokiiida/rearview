#!/usr/bin/env bash
set -euo pipefail

if ! command -v nix >/dev/null 2>&1; then
  echo "ERROR: Nix is not installed."
  exit 1
fi

echo "Updating flake.lock..."
nix flake update

echo "Verifying build..."
nix build --no-link --print-build-logs

echo "Verifying binary..."
nix run . -- --version

git add flake.lock

echo ""
echo "flake.lock updated and Nix build verified."
echo "Review with 'git diff --cached' and commit when ready."
