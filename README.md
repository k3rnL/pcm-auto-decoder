# PCM Auto Decoder

`pcm-auto-decoder` is Open Cinema's adaptive S/PDIF processor. It receives a
PCM or IEC-61937 carrier, detects content changes, decodes supported compressed
formats with the system FFmpeg libraries, and always exposes one stable PCM
output to PipeWire.

The default emitted bus is float32, 48 kHz, 7.1 in this explicit Open Cinema
channel order:

```text
FL FR FC LFE SL SR RL RR
```

The output node, sample format, rate, layout, and channel positions do not
change when content moves between a stereo menu and 5.1 or 7.1 movies. Missing
positions are silent. This stable contract lets Open Cinema keep the downstream
CamillaDSP and speaker graph linked while only the decoder's internal mode
changes.

## Supported content and layouts

| Input content | Behavior | Stable output |
| --- | --- | --- |
| PCM | Position-preserving format conversion and channel expansion | Configured PCM bus |
| IEC-61937 AC-3 | Decode with FFmpeg, resample/reorder, expand missing positions | Configured PCM bus |
| IEC-61937 E-AC-3 | Decode with FFmpeg, resample/reorder, expand missing positions | Configured PCM bus |
| IEC-61937 DTS types I-III | Decode with FFmpeg, resample/reorder, expand missing positions | Configured PCM bus |
| Unsupported IEC-61937 data type | Report a recoverable error and emit silence | Configured PCM bus |

Explicit layouts are `mono`, `stereo`, `5.1-side`, `5.1-rear`, and `7.1`.
The aliases `2.0`, `5.1`, and `5.1-back` are accepted. A bare six-channel
layout is rejected because it does not say whether the final pair is side or
rear. Narrowing is also rejected: an intentional downmix belongs in a
downstream processor such as CamillaDSP.

The S/PDIF/IEC-61937 transport is normally stereo S16LE at 48 kHz. Other
explicit PCM carrier formats can be configured, but input and output sample
rates must currently agree.

## Native PipeWire architecture

Normal mode creates exactly two native PipeWire nodes for each managed
instance:

- `open-cinema.decoder.<instance>.capture`
- `open-cinema.decoder.<instance>.output`

The nodes share `node.group`, set `node.autoconnect=false`, and do not select a
target. WirePlumber and Open Cinema own every external link. The decoder owns
only its native capture/output streams and bounded real-time queues. PipeWire
does not launch the process: Open Cinema's processor manager (or a systemd unit
for a standalone installation) starts and supervises each instance.

PipeWire callbacks only move preallocated bounded buffers. Detection, FFmpeg
decode/resampling, file access, and status clients stay outside the real-time
callback. An output underrun produces silence rather than changing the graph.
There is no PulseAudio client or compatibility-layer dependency.

```bash
pcm-auto-decoder \
  --instance-id living-room \
  --status-socket /run/open-cinema/decoder/living-room.sock \
  --capture-format S16LE \
  --capture-rate 48000 \
  --capture-layout stereo \
  --output-format F32LE \
  --output-rate 48000 \
  --output-layout 7.1
```

Run `pcm-auto-decoder --help` for detection-window and queue-related options.

## Status protocol

Managed processes expose protocol-v2 status snapshots and events through a
local NDJSON Unix socket. Messages describe the carrier, detected codec, actual
FFmpeg decoded-frame format, stable emitted format, confidence, lifecycle,
PipeWire stream identities, and bounded queue diagnostics. Consumers must
reject unknown protocol versions.

See [docs/STATUS_PROTOCOL.md](docs/STATUS_PROTOCOL.md) for the complete
contract and examples.

## Offline fixture mode

Supplying both files disables PipeWire while preserving the same detection and
single-output contract. Input is headerless raw audio matching the configured
capture format; output is headerless raw PCM matching the configured output.

```bash
pcm-auto-decoder \
  --capture-file carrier.s16le \
  --output-file adaptive-output.f32le \
  --loop-capture-file \
  --capture-format S16LE \
  --capture-rate 48000 \
  --capture-layout stereo \
  --output-format F32LE \
  --output-rate 48000 \
  --output-layout 7.1
```

`--loop-capture-file` rewinds a non-empty fixture at EOF. It is intended for
repeatable development, CI, and Open Cinema fake-device tests. Send `SIGINT` or
`SIGTERM` to stop a looping process cleanly.

## Supported release platforms

Release artifacts target Debian 13 (Trixie) GNU/Linux on:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu` (the Raspberry Pi 5 appliance target)

They dynamically link Debian's native PipeWire and FFmpeg libraries. The
minimum runtime packages on Trixie are:

```bash
sudo apt-get install \
  libavcodec61 \
  libavutil59 \
  libpipewire-0.3-0 \
  libswresample5
```

The matching source-build packages are:

```bash
sudo apt-get install \
  cargo \
  rustc \
  clang \
  pkg-config \
  python3 \
  libavcodec-dev \
  libavformat-dev \
  libavutil-dev \
  libclang-dev \
  libpipewire-0.3-dev \
  libswresample-dev
```

Rust 1.85 or newer is required for the Rust 2024 edition. Dependencies are
resolved from the committed `Cargo.lock`; development and release validation
use the Rust 1.98.0 toolchain pinned in `rust-toolchain.toml`, and CI commands
use `--locked`.

## Development container

The devcontainer is Debian Trixie with stable Rust, Clang, native PipeWire
headers, and system FFmpeg headers/libraries. It does not start an audio daemon or
D-Bus service. Offline tests work without a PipeWire session; live-node testing
requires deliberately exposing a compatible host PipeWire socket and runtime
directory to the container.

Open the repository in the devcontainer, then run:

```bash
cargo test --locked
cargo build --release --locked
scripts/smoke-offline.sh target/release/pcm-auto-decoder
```

## Validation

The same gate script is used by branch/PR CI and the tag workflow. Select the
native target for the machine running it:

```bash
# Debian Trixie x86_64
scripts/ci-gates.sh x86_64-unknown-linux-gnu

# Debian Trixie AArch64
scripts/ci-gates.sh aarch64-unknown-linux-gnu
```

That gate requires all of the following:

- `cargo fmt --all -- --check`
- Clippy with warnings denied and locked dependency resolution
- the complete locked test suite, including offline/status fixtures
- a locked release build for the native target
- Cargo manifest, lockfile, and binary version agreement
- ELF architecture and direct PipeWire/FFmpeg linkage checks
- rejection of any PulseAudio dependency in the runtime closure
- a bounded finite/looping offline decode smoke

CI runs the complete gate natively on Debian Trixie x86_64 and AArch64. The
AArch64 release is therefore linked against the same operating-system ABI used
by the appliance, rather than an undeclared cross sysroot.

## Installing a release

Each `vX.Y.Z` release publishes one archive per target:

```text
pcm-auto-decoder-vX.Y.Z-debian-trixie-x86_64-unknown-linux-gnu.tar.gz
pcm-auto-decoder-vX.Y.Z-debian-trixie-aarch64-unknown-linux-gnu.tar.gz
```

Each archive has a sibling `.sha256` file and a portable
`.provenance.json` record containing the source tag/commit, workflow run,
target, build environment, runtime-library contract, and artifact digest.
Verify the downloaded bytes before installing them:

```bash
sha256sum --check pcm-auto-decoder-vX.Y.Z-debian-trixie-<target>.tar.gz.sha256
python3 -m json.tool \
  pcm-auto-decoder-vX.Y.Z-debian-trixie-<target>.tar.gz.provenance.json >/dev/null
tar -xzf pcm-auto-decoder-vX.Y.Z-debian-trixie-<target>.tar.gz
sudo install -m 0755 \
  pcm-auto-decoder-vX.Y.Z-debian-trixie-<target>/pcm-auto-decoder \
  /usr/local/bin/pcm-auto-decoder
pcm-auto-decoder --version
```

Open Cinema deployment verifies the same digest and installs the selected
target through its coordinated release manifest.

## Versioning and releases

The package follows SemVer. `Cargo.toml` is the version source of truth;
`Cargo.lock`, `pcm-auto-decoder --version`, the Git tag `v<version>`, GitHub
release title, and every archive name must agree. `scripts/verify-version.py`
enforces this contract.

Release preparation is intentionally separate from tagging:

1. Update the Cargo version and regenerate `Cargo.lock`.
2. Run both architecture gates and merge through the normal branch/PR CI.
3. Create and push the immutable `v<version>` tag at the accepted commit.
4. The tag workflow rebuilds both targets natively on Trixie with `--locked`.
5. It publishes target-qualified archives, SHA-256 files, and provenance.
6. Native Trixie jobs download the published bytes, verify their identity and
   linkage, and run the bounded offline fixture smoke again.

A failed published tag is never moved or reused; a corrected release receives
a new version.
