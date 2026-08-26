# Decoder status protocol v2

The decoder exposes complete newline-delimited JSON documents over a Unix
socket configured with `--status-socket`. Open Cinema owns lifecycle and graph
links; the socket is observation-only.

A client requests the current complete state after connecting or after a
sequence gap:

```json
{"protocolVersion":2,"messageType":"getStatus"}
```

The response and every state-change event have the same complete shape:

```json
{
  "protocolVersion": 2,
  "messageType": "status",
  "instanceId": "living-room",
  "sequence": 17,
  "timestamp": "2026-08-22T21:04:05.123Z",
  "lifecycle": "ready",
  "mode": "decoding",
  "transport": {
    "framing": "iec61937",
    "sampleRate": 48000,
    "sampleFormat": "s16le",
    "channels": 2,
    "channelLayout": "stereo"
  },
  "codec": "ac3",
  "decoded": {
    "sampleRate": 48000,
    "sampleFormat": "f32(planar)",
    "channels": 6,
    "channelLayout": "5.1"
  },
  "emitted": {
    "sampleRate": 48000,
    "sampleFormat": "float32le",
    "channels": 8,
    "channelLayout": "7.1"
  },
  "confidence": {
    "score": 1.0,
    "observations": 2,
    "requiredObservations": 2
  },
  "streams": {
    "captureNodeName": "open-cinema.decoder.living-room.capture",
    "captureStreamName": "open-cinema.decoder.living-room.capture.stream",
    "outputNodeName": "open-cinema.decoder.living-room.output",
    "outputStreamName": "open-cinema.decoder.living-room.output.stream",
    "nodeGroupName": "open-cinema.decoder.living-room"
  },
  "errors": []
}
```

`transport` describes the carrier. `codec` describes detected encoded content.
`decoded` is the actual frame reported by FFmpeg before normalization and is
null for PCM or while no frame exists. `emitted` is always present and describes
the stable adaptive output contract. It does not change merely because content
changes.

`lifecycle` is `starting`, `ready`, `stopping`, or `failed`. `mode` is
`unknown`, `detecting`, `pcm`, `decoding`, or `error`. Unknown/detecting/error
windows emit silence. Queue overflow and underrun reports use structured
`{code, message, recoverable}` entries.

`sequence` is monotonic for one process. Consumers correlate it with
`instanceId` and their process/connection generation. A reconnect starts with
`getStatus`; there is no event replay.

Protocol v2 intentionally replaces the provisional split-output v1 contract.
Consumers must reject all other protocol versions.
