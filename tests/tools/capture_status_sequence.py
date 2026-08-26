#!/usr/bin/env python3
"""Capture distinct decoder status updates from the Unix status socket."""

from __future__ import annotations

import argparse
import json
import socket
import time
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("socket", type=Path)
    parser.add_argument("--duration", type=float, default=3.0)
    args = parser.parse_args()

    deadline = time.monotonic() + args.duration
    statuses: list[dict[str, object]] = []
    sequences: set[int] = set()
    last_state: str | None = None
    errors: dict[str, dict[str, object]] = {}
    pending = b""

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(0.1)
        client.connect(str(args.socket))
        client.sendall(b'{"protocolVersion":2,"messageType":"getStatus"}\n')

        while time.monotonic() < deadline:
            try:
                chunk = client.recv(65536)
            except TimeoutError:
                continue
            if not chunk:
                break
            pending += chunk
            while b"\n" in pending:
                raw_line, pending = pending.split(b"\n", 1)
                if not raw_line:
                    continue
                message = json.loads(raw_line)
                if message.get("messageType") != "status":
                    continue
                sequence = int(message["sequence"])
                if sequence not in sequences:
                    sequences.add(sequence)
                    state = json.dumps(
                        [
                            message["lifecycle"],
                            message["mode"],
                            message["transport"]["framing"],
                            message["codec"],
                            message["decoded"],
                            message["emitted"],
                        ],
                        sort_keys=True,
                    )
                    if state != last_state:
                        statuses.append(message)
                        last_state = state
                    for error in message["errors"]:
                        current = errors.setdefault(
                            error["code"],
                            {
                                "code": error["code"],
                                "recoverable": error["recoverable"],
                                "lastMessage": error["message"],
                                "statusCount": 0,
                            },
                        )
                        current["lastMessage"] = error["message"]
                        current["statusCount"] = int(current["statusCount"]) + 1

    summary = [
        {
            "sequence": status["sequence"],
            "lifecycle": status["lifecycle"],
            "mode": status["mode"],
            "transport": status["transport"]["framing"],
            "codec": status["codec"],
            "decoded": status["decoded"],
            "emitted": status["emitted"],
        }
        for status in statuses
    ]
    print(
        json.dumps(
            {
                "states": summary,
                "errors": list(errors.values()),
                "observedStatusCount": len(sequences),
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
