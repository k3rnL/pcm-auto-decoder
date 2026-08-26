#!/usr/bin/env bash
set -euo pipefail

target=${1:?usage: package-release.sh TARGET TAG BINARY OUTPUT_DIRECTORY}
tag=${2:?usage: package-release.sh TARGET TAG BINARY OUTPUT_DIRECTORY}
binary=${3:?usage: package-release.sh TARGET TAG BINARY OUTPUT_DIRECTORY}
output_directory=${4:?usage: package-release.sh TARGET TAG BINARY OUTPUT_DIRECTORY}

project=pcm-auto-decoder
version=${tag#v}
package_root=${project}-${tag}-debian-trixie-${target}
archive_name=${package_root}.tar.gz
repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

case "${target}" in
    x86_64-unknown-linux-gnu)
        expected_build_architecture=x86_64
        ;;
    aarch64-unknown-linux-gnu)
        expected_build_architecture=aarch64
        ;;
    *)
        echo "unsupported release target: ${target}" >&2
        exit 1
        ;;
esac
if [[ "$(uname -m)" != "${expected_build_architecture}" ]]; then
    echo "release archive must be packaged in a native ${expected_build_architecture} environment" >&2
    exit 1
fi

stage=$(mktemp -d)
cleanup() {
    rm -rf "${stage}"
}
trap cleanup EXIT

python3 "${repository_root}/scripts/verify-version.py" \
    --binary "${binary}" \
    --tag "${tag}" \
    --archive-name "${archive_name}" \
    --target "${target}"
"${repository_root}/scripts/verify-native-binary.sh" "${binary}" "${target}"

mkdir -p "${stage}/${package_root}/docs" "${output_directory}"
install -m 0755 "${binary}" "${stage}/${package_root}/${project}"
install -m 0644 "${repository_root}/README.md" "${stage}/${package_root}/README.md"
install -m 0644 "${repository_root}/docs/STATUS_PROTOCOL.md" \
    "${stage}/${package_root}/docs/STATUS_PROTOCOL.md"

source_date_epoch=${SOURCE_DATE_EPOCH:-$(git -C "${repository_root}" -c "safe.directory=${repository_root}" show -s --format=%ct HEAD)}
tar \
    --sort=name \
    --mtime="@${source_date_epoch}" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "${stage}" \
    -cf - "${package_root}" \
    | gzip -n >"${output_directory}/${archive_name}"

(
    cd "${output_directory}"
    sha256sum "${archive_name}" >"${archive_name}.sha256"
)

python3 - \
    "${output_directory}/${archive_name}" \
    "${output_directory}/${archive_name}.provenance.json" \
    "${project}" \
    "${version}" \
    "${tag}" \
    "${target}" \
    "${GITHUB_REPOSITORY:-k3rnL/pcm-auto-decoder}" \
    "${GITHUB_SHA:-$(git -C "${repository_root}" -c "safe.directory=${repository_root}" rev-parse HEAD)}" \
    "${GITHUB_WORKFLOW:-local}" \
    "${GITHUB_RUN_ID:-local}" \
    "${GITHUB_RUN_ATTEMPT:-1}" <<'PY'
import datetime
import hashlib
import json
import os
import pathlib
import platform
import subprocess
import sys

(
    archive_path,
    provenance_path,
    project,
    version,
    tag,
    target,
    repository,
    commit,
    workflow,
    run_id,
    run_attempt,
) = sys.argv[1:]
archive = pathlib.Path(archive_path)
os_release = {}
for line in pathlib.Path("/etc/os-release").read_text().splitlines():
    if "=" in line:
        key, value = line.split("=", 1)
        os_release[key] = value.strip('"')
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
run_url = (
    f"https://github.com/{repository}/actions/runs/{run_id}"
    if run_id != "local"
    else None
)
document = {
    "schemaVersion": 1,
    "project": project,
    "version": version,
    "tag": tag,
    "source": {"repository": repository, "commit": commit},
    "build": {
        "workflow": workflow,
        "runId": run_id,
        "runAttempt": run_attempt,
        "runUrl": run_url,
        "operatingSystem": os_release.get("PRETTY_NAME"),
        "codename": os_release.get("VERSION_CODENAME"),
        "architecture": platform.machine(),
        "target": target,
        "rustc": subprocess.run(
            ["rustc", "--version"], check=True, text=True, capture_output=True
        ).stdout.strip(),
    },
    "runtimeContract": {
        "operatingSystem": "debian-trixie",
        "libraries": [
            "libpipewire-0.3.so",
            "libavcodec.so",
            "libavutil.so",
            "libswresample.so",
        ],
        "forbiddenLibraries": ["libpulse.so", "libpulse-simple.so"],
    },
    "artifact": {"name": archive.name, "sha256": digest},
    "generatedAt": datetime.datetime.now(datetime.timezone.utc)
    .replace(microsecond=0)
    .isoformat(),
}
pathlib.Path(provenance_path).write_text(
    json.dumps(document, indent=2, sort_keys=True) + "\n"
)
PY

printf 'packaged %s\n' "${output_directory}/${archive_name}"
