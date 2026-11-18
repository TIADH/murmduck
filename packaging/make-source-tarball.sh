#!/usr/bin/env bash
# Build the self-contained source tarball consumed by packaging/murmduck.spec.
#
# The tarball contains the crate sources, Cargo.lock, docs and a `vendor/`
# directory with every Rust dependency, plus `.cargo/config.toml` telling cargo
# to use it. This lets `rpmbuild` build and test fully offline.
#
# Usage (from anywhere in a checkout):
#   ./packaging/make-source-tarball.sh [VERSION]
#
# Requires: cargo, tar, mktemp, network access to crates.io on first vendor.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"

version="${1:-$(awk -F'"' '/^version = / { print $2; exit }' "${repo_root}/Cargo.toml")}"
if [[ -z "${version}" ]]; then
    echo "error: could not determine version from Cargo.toml" >&2
    exit 1
fi

archive_name="murmduck-${version}"
stage="$(mktemp -d)"
trap 'rm -rf "${stage}"' EXIT

src="${stage}/${archive_name}"
mkdir -p "${src}/.cargo"

# Copy the files the spec builds, tests and installs.
for item in Cargo.toml Cargo.lock LICENSE README.md docs packaging src tests examples; do
    cp -a -- "${repo_root}/${item}" "${src}/"
done

# Vendor all dependencies (including dev-dependencies needed by %check) and
# write the cargo config that redirects crates.io to the local directory.
(
    cd -- "${src}"
    cargo vendor --locked vendor > .cargo/config.toml
)

tar -czf "${repo_root}/${archive_name}.tar.gz" -C "${stage}" "${archive_name}"
echo "created ${repo_root}/${archive_name}.tar.gz"
