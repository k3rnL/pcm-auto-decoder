#!/usr/bin/env python3
"""Build a deterministic stereo S16 IEC-61937/PCM transition carrier."""

from __future__ import annotations

import argparse
import struct
from pathlib import Path


def frames(path: Path, frame_bytes: int, syncword: bytes) -> tuple[bytes, ...]:
    payload = path.read_bytes()
    if not payload or len(payload) % frame_bytes:
        raise ValueError(f"{path} is not a whole number of {frame_bytes}-byte frames")
    values = tuple(
        payload[offset : offset + frame_bytes]
        for offset in range(0, len(payload), frame_bytes)
    )
    if any(not frame.startswith(syncword) for frame in values):
        raise ValueError(f"{path} contains a frame without the expected syncword")
    return values


def swapped_words(payload: bytes) -> bytes:
    if len(payload) % 2:
        payload += b"\0"
    return b"".join(payload[index : index + 2][::-1] for index in range(0, len(payload), 2))


def burst(payload: bytes, *, data_type: int, carrier_bytes: int) -> bytes:
    body = swapped_words(payload)
    header = struct.pack("<HHHH", 0xF872, 0x4E1F, data_type, len(payload) * 8)
    if len(header) + len(body) > carrier_bytes:
        raise ValueError("encoded frame does not fit its IEC-61937 carrier period")
    return header + body + bytes(carrier_bytes - len(header) - len(body))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ac3", type=Path, required=True)
    parser.add_argument("--dts", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--ac3-frame-bytes", type=int, default=1792)
    parser.add_argument("--dts-frame-bytes", type=int, default=1884)
    parser.add_argument("--pcm-milliseconds", type=int, default=250)
    arguments = parser.parse_args()

    pcm_bytes = 48_000 * arguments.pcm_milliseconds // 1_000 * 2 * 2
    pcm = bytes(pcm_bytes)
    ac3 = b"".join(
        burst(frame, data_type=0x01, carrier_bytes=1536 * 2 * 2)
        for frame in frames(arguments.ac3, arguments.ac3_frame_bytes, b"\x0b\x77")
    )
    dts = b"".join(
        burst(frame, data_type=0x0B, carrier_bytes=512 * 2 * 2)
        for frame in frames(arguments.dts, arguments.dts_frame_bytes, b"\x7f\xfe\x80\x01")
    )
    arguments.output.write_bytes(pcm + ac3 + pcm + dts)
    print(
        f"wrote {arguments.output}: pcm={len(pcm) * 2} ac3={len(ac3)} "
        f"dts={len(dts)} total={arguments.output.stat().st_size}"
    )


if __name__ == "__main__":
    main()
