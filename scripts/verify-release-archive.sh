#!/usr/bin/env bash
set -euo pipefail

download_directory=${1:?usage: verify-release-archive.sh DIRECTORY TARGET TAG}
target=${2:?usage: verify-release-archive.sh DIRECTORY TARGET TAG}
tag=${3:?usage: verify-release-archive.sh DIRECTORY TARGET TAG}

project=pcm-auto-decoder
package_root=${project}-${tag}-debian-trixie-${target}
archive_name=${package_root}.tar.gz
archive=${download_directory}/${archive_name}
checksum=${archive}.sha256
provenance=${archive}.provenance.json

for required in "${archive}" "${checksum}" "${provenance}"; do
    if [[ ! -f "${required}" ]]; then
        echo "missing release asset: ${required}" >&2
        exit 1
    fi
done

(
    cd "${download_directory}"
    sha256sum --check "${archive_name}.sha256"
)

python3 - "${provenance}" "${archive}" "${target}" "${tag}" <<'PY'
import hashlib
import json
import os
import pathlib
import sys

provenance_path, archive_path, target, tag = sys.argv[1:]
document = json.loads(pathlib.Path(provenance_path).read_text())
archive = pathlib.Path(archive_path)
expected = {
    "project": "pcm-auto-decoder",
    "version": tag.removeprefix("v"),
    "tag": tag,
}
for key, value in expected.items():
    if document.get(key) != value:
        raise SystemExit(f"provenance {key} is {document.get(key)!r}, expected {value!r}")
if document.get("build", {}).get("target") != target:
    raise SystemExit("provenance target does not match downloaded archive")
expected_architecture = {
    "x86_64-unknown-linux-gnu": "x86_64",
    "aarch64-unknown-linux-gnu": "aarch64",
}[target]
if document.get("build", {}).get("architecture") != expected_architecture:
    raise SystemExit("provenance architecture does not prove a native target build")
if document.get("build", {}).get("codename") != "trixie":
    raise SystemExit("provenance does not identify a Debian Trixie build")
if document.get("artifact", {}).get("name") != archive.name:
    raise SystemExit("provenance artifact name does not match downloaded archive")
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
if document.get("artifact", {}).get("sha256") != digest:
    raise SystemExit("provenance digest does not match downloaded archive")
expected_repository = os.environ.get("GITHUB_REPOSITORY")
expected_commit = os.environ.get("GITHUB_SHA")
if expected_repository and document.get("source", {}).get("repository") != expected_repository:
    raise SystemExit("provenance repository does not match workflow repository")
if expected_commit and document.get("source", {}).get("commit") != expected_commit:
    raise SystemExit("provenance commit does not match tag commit")
if not document.get("build", {}).get("workflow"):
    raise SystemExit("provenance has no workflow identity")
PY

workdir=$(mktemp -d)
cleanup() {
    rm -rf "${workdir}"
}
trap cleanup EXIT

tar -xzf "${archive}" -C "${workdir}"
binary=${workdir}/${package_root}/${project}
test -f "${workdir}/${package_root}/README.md"
test -f "${workdir}/${package_root}/docs/STATUS_PROTOCOL.md"

python3 scripts/verify-version.py \
    --binary "${binary}" \
    --tag "${tag}" \
    --archive-name "${archive_name}" \
    --target "${target}"
scripts/verify-native-binary.sh "${binary}" "${target}"
scripts/smoke-offline.sh "${binary}"

printf 'verified downloaded release archive %s\n' "${archive_name}"
