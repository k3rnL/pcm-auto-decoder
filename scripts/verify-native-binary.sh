#!/usr/bin/env bash
set -euo pipefail

binary=${1:?usage: verify-native-binary.sh BINARY TARGET}
target=${2:?usage: verify-native-binary.sh BINARY TARGET}

test -x "${binary}"

case "${target}" in
    x86_64-unknown-linux-gnu)
        expected_file_pattern='x86-64'
        ;;
    aarch64-unknown-linux-gnu)
        expected_file_pattern='ARM aarch64'
        ;;
    *)
        echo "unsupported release target: ${target}" >&2
        exit 1
        ;;
esac

file_output=$(file "${binary}")
grep -Fq "${expected_file_pattern}" <<<"${file_output}" || {
    echo "binary architecture does not match ${target}: ${file_output}" >&2
    exit 1
}

needed=$(readelf -d "${binary}" | grep 'NEEDED')
for library in \
    libpipewire-0.3.so \
    libavcodec.so \
    libavutil.so \
    libswresample.so
do
    grep -Fq "${library}" <<<"${needed}" || {
        echo "release binary does not directly link required ${library}" >&2
        exit 1
    }
done

if grep -Eqi 'libpulse|pulse-simple' <<<"${needed}"; then
    echo "release binary must not link PulseAudio" >&2
    exit 1
fi

linkage=$(ldd "${binary}")
if grep -Fq 'not found' <<<"${linkage}"; then
    echo "release binary has unresolved runtime libraries:" >&2
    echo "${linkage}" >&2
    exit 1
fi
if grep -Eqi 'libpulse|pulse-simple' <<<"${linkage}"; then
    echo "release runtime dependency closure must not contain PulseAudio" >&2
    exit 1
fi

printf 'verified native linkage for %s (%s)\n' "${binary}" "${target}"
