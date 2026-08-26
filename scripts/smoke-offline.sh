#!/usr/bin/env bash
set -euo pipefail

binary=${1:?usage: smoke-offline.sh BINARY}
test -x "${binary}"

workdir=$(mktemp -d)
loop_pid=''
cleanup() {
    if [[ -n "${loop_pid}" ]] && kill -0 "${loop_pid}" 2>/dev/null; then
        kill -TERM "${loop_pid}" 2>/dev/null || true
        wait "${loop_pid}" 2>/dev/null || true
    fi
    rm -rf "${workdir}"
}
trap cleanup EXIT

capture=${workdir}/stereo-s16le.raw
finite_output=${workdir}/finite.f32le
loop_output=${workdir}/loop.f32le
loop_log=${workdir}/loop.log

# Sixty-four silent stereo S16LE frames are a deterministic PCM carrier fixture.
dd if=/dev/zero of="${capture}" bs=256 count=1 status=none

"${binary}" \
    --capture-file "${capture}" \
    --output-file "${finite_output}" \
    --chunk-frames 64 \
    --capture-format S16LE \
    --capture-rate 48000 \
    --capture-layout stereo \
    --output-format F32LE \
    --output-rate 48000 \
    --output-layout 7.1

finite_size=$(stat -c %s "${finite_output}")
if (( finite_size == 0 || finite_size % 32 != 0 )); then
    echo "finite offline output is empty or not aligned to 7.1 F32LE frames" >&2
    exit 1
fi

"${binary}" \
    --capture-file "${capture}" \
    --output-file "${loop_output}" \
    --loop-capture-file \
    --chunk-frames 64 \
    --capture-format S16LE \
    --capture-rate 48000 \
    --capture-layout stereo \
    --output-format F32LE \
    --output-rate 48000 \
    --output-layout 7.1 >"${loop_log}" 2>&1 &
loop_pid=$!

looped=false
for _ in $(seq 1 200); do
    if ! kill -0 "${loop_pid}" 2>/dev/null; then
        wait "${loop_pid}" || true
        echo "looping offline decoder exited before replaying the fixture" >&2
        sed -n '1,120p' "${loop_log}" >&2
        exit 1
    fi
    if [[ -f "${loop_output}" ]]; then
        loop_size=$(stat -c %s "${loop_output}")
        if (( loop_size > finite_size )); then
            looped=true
            break
        fi
    fi
    sleep 0.01
done

if [[ "${looped}" != true ]]; then
    echo "looping offline decoder did not emit more than one fixture pass" >&2
    exit 1
fi

kill -TERM "${loop_pid}"
wait "${loop_pid}"
loop_pid=''

loop_size=$(stat -c %s "${loop_output}")
if (( loop_size % 32 != 0 )); then
    echo "looping offline output is not aligned to 7.1 F32LE frames" >&2
    exit 1
fi

printf 'verified bounded offline fixture and looping behavior (%d -> %d bytes)\n' \
    "${finite_size}" "${loop_size}"
