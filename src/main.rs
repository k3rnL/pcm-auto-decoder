mod decoders;
mod detection;
mod iec61937_detector;
mod pipewire_io;
mod sinks;
mod status_protocol;

use crate::decoders::Ac3DecoderSink;
use crate::detection::{Codec, DetectionTracker, StableMode};
use crate::iec61937_detector::Iec61937Detector;
use crate::pipewire_io::{PipeWireInput, PipeWireRuntime};
use crate::sinks::{AudioFormat, AudioLayout, AudioSink, AudioSpec, FileSink, PcmNormalizer};
use crate::status_protocol::{
    AudioDescriptor, DecoderStatus, DetectionMode, Lifecycle, StatusError, StatusReporter,
    StatusServer, TransportDescriptor, TransportFraming,
};
use anyhow::{Context, Result};
use clap::Parser;
use ffmpeg_next::codec::Id;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const DEFAULT_CHUNK_FRAMES: usize = 512;
const DEFAULT_DET_WINDOW_MS: u64 = 250;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Native PipeWire IEC-61937 detector/decoder with one stable adaptive PCM output"
)]
struct Args {
    /// Offline capture file. Requires --output-file; otherwise native PipeWire is used.
    #[arg(long, value_name = "PATH")]
    capture_file: Option<PathBuf>,

    /// Loop the offline capture file at EOF.
    #[arg(long)]
    loop_capture_file: bool,

    /// Offline adaptive PCM output file. Requires --capture-file.
    #[arg(long, value_name = "PATH")]
    output_file: Option<PathBuf>,

    /// Native capture/offline carrier layout.
    #[arg(long, default_value = "stereo")]
    capture_layout: String,

    /// Native capture/offline carrier rate.
    #[arg(long, default_value_t = 48_000)]
    capture_rate: u32,

    /// Native capture/offline carrier sample format.
    #[arg(long, default_value = "S16LE")]
    capture_format: String,

    /// Stable adaptive output layout. Six-channel values must say 5.1-side or 5.1-rear.
    #[arg(long, default_value = "7.1")]
    output_layout: String,

    /// Stable adaptive output rate.
    #[arg(long, default_value_t = 48_000)]
    output_rate: u32,

    /// Stable adaptive output sample format.
    #[arg(long, default_value = "F32LE")]
    output_format: String,

    /// Frames per offline read. PipeWire chooses its graph quantum independently.
    #[arg(long, default_value_t = DEFAULT_CHUNK_FRAMES)]
    chunk_frames: usize,

    /// Milliseconds without IEC-61937 before switching to PCM (and vice-versa).
    #[arg(long, default_value_t = DEFAULT_DET_WINDOW_MS)]
    det_window_ms: u64,

    /// Legacy chunk-count detection window. Prefer --det-window-ms.
    #[arg(long, hide = true)]
    det_window: Option<usize>,

    /// Stable Open Cinema identity used in status and native PipeWire properties.
    #[arg(long, default_value = "standalone")]
    instance_id: String,

    /// Unix socket for the versioned newline-delimited JSON status protocol.
    #[arg(long, value_name = "PATH")]
    status_socket: Option<PathBuf>,

    /// Matching IEC-61937 bursts required before activating or changing a decoder.
    #[arg(long, default_value_t = 2)]
    encoded_confirmations: usize,
}

enum Input {
    PipeWire(PipeWireInput),
    File {
        file: File,
        buffer: Vec<u8>,
        loop_at_eof: bool,
    },
}

impl Input {
    fn open_file(
        path: &Path,
        spec: AudioSpec,
        chunk_frames: usize,
        loop_at_eof: bool,
    ) -> Result<Self> {
        anyhow::ensure!(chunk_frames > 0, "chunk_frames must be positive");
        let file = File::open(path).context("open capture file")?;
        anyhow::ensure!(
            !loop_at_eof || file.metadata().context("inspect capture file")?.len() > 0,
            "cannot loop an empty capture file"
        );
        Ok(Self::File {
            file,
            buffer: vec![0; chunk_frames * spec.frame_bytes()],
            loop_at_eof,
        })
    }

    fn read_chunk(&mut self, stop: &AtomicBool) -> Result<Option<&[u8]>> {
        match self {
            Self::PipeWire(input) => input.read_chunk(stop),
            Self::File {
                file,
                buffer,
                loop_at_eof,
            } => {
                let mut received = 0;
                while received < buffer.len() && !stop.load(Ordering::Acquire) {
                    let count = file
                        .read(&mut buffer[received..])
                        .context("read capture file")?;
                    if count == 0 {
                        if !*loop_at_eof {
                            if received == 0 {
                                return Ok(None);
                            }
                            buffer[received..].fill(0);
                            break;
                        }
                        file.seek(SeekFrom::Start(0))
                            .context("rewind capture file")?;
                        anyhow::ensure!(
                            file.stream_position().context("inspect capture file")? == 0,
                            "capture file could not be rewound"
                        );
                    } else {
                        received += count;
                    }
                }
                if stop.load(Ordering::Acquire) {
                    Ok(None)
                } else {
                    Ok(Some(buffer.as_slice()))
                }
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let capture_spec = audio_spec(
        &args.capture_format,
        args.capture_rate,
        &args.capture_layout,
    )?;
    let output_spec = audio_spec(&args.output_format, args.output_rate, &args.output_layout)?;
    anyhow::ensure!(
        output_spec
            .layout
            .accepts_position_preserving_input(capture_spec.layout),
        "adaptive output layout would narrow the capture carrier"
    );
    anyhow::ensure!(
        args.capture_file.is_some() == args.output_file.is_some(),
        "offline mode requires both --capture-file and --output-file"
    );

    let reporter = StatusReporter::new(DecoderStatus::starting(
        args.instance_id.clone(),
        transport_descriptor(capture_spec),
        audio_descriptor(output_spec),
        args.encoded_confirmations.max(1) as u32,
    ));
    let _status_server = args
        .status_socket
        .as_ref()
        .map(|path| StatusServer::start(path, reporter.clone()))
        .transpose()
        .context("starting decoder status socket")?;

    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&stop))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&stop))?;

    match run(&args, capture_spec, output_spec, &reporter, &stop) {
        Ok(()) => {
            reporter.update(|status| status.lifecycle = Lifecycle::Stopping);
            Ok(())
        }
        Err(error) => {
            reporter.update(|status| {
                status.lifecycle = Lifecycle::Failed;
                status.mode = DetectionMode::Error;
                push_status_error(
                    status,
                    StatusError {
                        code: "decoder_failure".to_owned(),
                        message: format!("{error:#}"),
                        recoverable: false,
                    },
                );
            });
            Err(error)
        }
    }
}

fn run(
    args: &Args,
    capture_spec: AudioSpec,
    output_spec: AudioSpec,
    reporter: &StatusReporter,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    ffmpeg_next::init().context("ffmpeg init failed")?;
    let streams = reporter.snapshot().streams;
    let (runtime, mut input, mut adaptive_sink): (
        Option<PipeWireRuntime>,
        Input,
        Option<Box<dyn AudioSink + Send>>,
    ) = if let (Some(capture_path), Some(output_path)) = (&args.capture_file, &args.output_file) {
        (
            None,
            Input::open_file(
                capture_path,
                capture_spec,
                args.chunk_frames,
                args.loop_capture_file,
            )?,
            Some(Box::new(FileSink::open(output_path, output_spec)?)),
        )
    } else {
        let (runtime, input, output) = PipeWireRuntime::open(
            &args.instance_id,
            &streams.capture_stream_name,
            &streams.capture_node_name,
            &streams.output_stream_name,
            &streams.output_node_name,
            &streams.node_group_name,
            capture_spec,
            output_spec,
            args.chunk_frames,
        )?;
        (
            Some(runtime),
            Input::PipeWire(input),
            Some(Box::new(output)),
        )
    };
    let pcm_normalizer = PcmNormalizer::new(capture_spec, output_spec)?;
    let mut decoder_sink: Option<Ac3DecoderSink> = None;
    let detection_window_frames = args
        .det_window
        .map(|chunks| chunks.saturating_mul(args.chunk_frames))
        .unwrap_or_else(|| {
            (u64::from(capture_spec.rate)
                .saturating_mul(args.det_window_ms)
                .saturating_add(999)
                / 1000) as usize
        })
        .max(1);
    let mut detector = DetectionTracker::new(detection_window_frames, args.encoded_confirmations);
    let mut candidate_buffer =
        Vec::with_capacity(detection_window_frames.saturating_mul(capture_spec.frame_bytes()));
    let candidate_buffer_limit = candidate_buffer.capacity().max(1);
    let mut last_diagnostics = (0, 0, 0);

    eprintln!(
        "Starting instance={} transport={} output={} offline={}",
        args.instance_id,
        capture_spec,
        output_spec,
        runtime.is_none(),
    );
    reporter.update(|status| {
        status.lifecycle = Lifecycle::Ready;
        status.mode = DetectionMode::Unknown;
    });

    while !stop.load(Ordering::Acquire) {
        let Some(chunk) = input.read_chunk(stop)?.map(ToOwned::to_owned) else {
            break;
        };
        if let Some(runtime) = &runtime {
            let diagnostics = runtime.diagnostics();
            publish_diagnostics(reporter, last_diagnostics, diagnostics);
            last_diagnostics = diagnostics;
        }

        let codec = Iec61937Detector::find_preamble(&chunk)
            .map(|preamble| Codec::from_stream_type(preamble.stream_type));
        let update = detector.observe_frames(codec, chunk.len() / capture_spec.frame_bytes());
        publish_detection(reporter, &update);
        if let Some(rejected) = update.rejected_candidate {
            reporter.update(|status| {
                push_status_error(
                    status,
                    StatusError {
                        code: "detection_candidate_rejected".to_owned(),
                        message: format!(
                            "ignored unconfirmed IEC-61937 candidate {}",
                            rejected.status_name()
                        ),
                        recoverable: true,
                    },
                );
            });
        }

        if let Some(transition) = update.transition {
            eprintln!(
                "Signal transition: {:?} -> {:?}",
                transition.previous, transition.current
            );
            if matches!(transition.previous, StableMode::Encoded(_)) {
                if let Some(decoder) = decoder_sink.take() {
                    adaptive_sink = Some(decoder.finish()?);
                }
            }
            if let Some(sink) = adaptive_sink.as_mut() {
                sink.flush()?;
            }
            if let StableMode::Encoded(codec) = transition.current {
                if let Some(codec_id) = ffmpeg_codec(codec) {
                    let observer_reporter = reporter.clone();
                    let observer = Box::new(move |descriptor: AudioDescriptor| {
                        if observer_reporter.snapshot().decoded.as_ref() != Some(&descriptor) {
                            observer_reporter.update(|status| status.decoded = Some(descriptor));
                        }
                    });
                    decoder_sink = Some(Ac3DecoderSink::wrap_with_observer(
                        adaptive_sink
                            .take()
                            .context("adaptive output sink not set")?,
                        capture_spec.format,
                        codec_id,
                        Some(observer),
                    )?);
                } else {
                    reporter.update(|status| {
                        status.mode = DetectionMode::Error;
                        push_status_error(
                            status,
                            StatusError {
                                code: "unsupported_codec".to_owned(),
                                message: format!(
                                    "unsupported IEC-61937 codec {}",
                                    codec.status_name()
                                ),
                                recoverable: true,
                            },
                        );
                    });
                }
            }
        }

        if update.buffer_current_chunk {
            candidate_buffer.extend_from_slice(&chunk);
            if candidate_buffer.len() > candidate_buffer_limit {
                let overflow = candidate_buffer.len() - candidate_buffer_limit;
                candidate_buffer.drain(..overflow);
            }
        }
        if update.flush_candidate_buffer {
            // Replaying the PCM confirmation window (or a rejected encoded
            // candidate) creates permanent A/V delay because capture and
            // playback then advance at the same rate. PipeWire already emits
            // silence while no decoded block is queued, so discard those old
            // bytes and resume from live input. A confirmed encoded candidate
            // is the only buffered data that must reach the codec decoder.
            if update.rejected_candidate.is_none()
                && matches!(update.stable_mode, StableMode::Encoded(_))
            {
                write_for_mode(
                    update.stable_mode,
                    &candidate_buffer,
                    &pcm_normalizer,
                    &mut adaptive_sink,
                    &mut decoder_sink,
                )?;
            }
            candidate_buffer.clear();
        }
        if !update.buffer_current_chunk {
            write_for_mode(
                update.stable_mode,
                &chunk,
                &pcm_normalizer,
                &mut adaptive_sink,
                &mut decoder_sink,
            )?;
        }
    }

    if let Some(decoder) = decoder_sink.take() {
        adaptive_sink = Some(decoder.finish()?);
    }
    if let Some(sink) = adaptive_sink.as_mut() {
        sink.flush()?;
    }
    Ok(())
}

fn write_for_mode(
    mode: StableMode,
    bytes: &[u8],
    pcm_normalizer: &PcmNormalizer,
    adaptive_sink: &mut Option<Box<dyn AudioSink + Send>>,
    decoder_sink: &mut Option<Ac3DecoderSink>,
) -> Result<()> {
    match mode {
        StableMode::Pcm => {
            let normalized = pcm_normalizer.convert(bytes)?;
            adaptive_sink
                .as_mut()
                .context("adaptive output sink not set")?
                .write(&normalized)?;
        }
        StableMode::Encoded(_) => {
            if let Some(decoder) = decoder_sink {
                decoder.write(bytes)?;
            } else if let Some(sink) = adaptive_sink {
                sink.write(&pcm_normalizer.silence_for_input(bytes))?;
            }
        }
        StableMode::Unknown => {
            if let Some(sink) = adaptive_sink {
                sink.write(&pcm_normalizer.silence_for_input(bytes))?;
            }
        }
    }
    Ok(())
}

fn audio_spec(format: &str, rate: u32, layout: &str) -> Result<AudioSpec> {
    AudioSpec {
        format: AudioFormat::parse(format)?,
        rate,
        layout: AudioLayout::parse(layout)?,
    }
    .validate()
}

fn audio_descriptor(spec: AudioSpec) -> AudioDescriptor {
    AudioDescriptor {
        sample_rate: spec.rate,
        sample_format: spec.format.status_name().to_owned(),
        channels: spec.channels() as u16,
        channel_layout: spec.layout.status_name().to_owned(),
    }
}

fn transport_descriptor(spec: AudioSpec) -> TransportDescriptor {
    TransportDescriptor {
        framing: TransportFraming::Unknown,
        sample_rate: spec.rate,
        sample_format: spec.format.status_name().to_owned(),
        channels: spec.channels() as u16,
        channel_layout: spec.layout.status_name().to_owned(),
    }
}

fn publish_detection(reporter: &StatusReporter, update: &detection::DetectionUpdate) {
    let codec = update.codec.map(Codec::status_name);
    let mode = if matches!(update.codec, Some(Codec::Unsupported(_))) {
        DetectionMode::Error
    } else {
        update.reported_mode.clone()
    };
    let current = reporter.snapshot();
    if current.mode == mode
        && current.transport.framing == update.framing
        && current.codec == codec
        && current.confidence == update.confidence
    {
        return;
    }
    let decoded_is_stale = mode != DetectionMode::Decoding || current.codec != codec;
    reporter.update(|status| {
        status.mode = mode;
        status.transport.framing = update.framing.clone();
        status.codec = codec;
        status.confidence = update.confidence.clone();
        if decoded_is_stale {
            status.decoded = None;
        }
        if status.mode != DetectionMode::Error {
            status
                .errors
                .retain(|error| error.code != "unsupported_codec");
        }
    });
}

fn publish_diagnostics(
    reporter: &StatusReporter,
    previous: (u64, u64, u64),
    current: (u64, u64, u64),
) {
    for (index, code, label) in [
        (0, "capture_queue_overflow", "capture queue overflows"),
        (1, "output_queue_underrun", "adaptive output underruns"),
        (
            2,
            "output_queue_overflow",
            "adaptive output queue overflows",
        ),
    ] {
        let old = [previous.0, previous.1, previous.2][index];
        let new = [current.0, current.1, current.2][index];
        if new > old {
            reporter.update(|status| {
                push_status_error(
                    status,
                    StatusError {
                        code: code.to_owned(),
                        message: format!("{label}: {new}"),
                        recoverable: true,
                    },
                )
            });
        }
    }
}

fn ffmpeg_codec(codec: Codec) -> Option<Id> {
    match codec {
        Codec::Ac3 => Some(Id::AC3),
        Codec::EAc3 => Some(Id::EAC3),
        Codec::Dts => Some(Id::DTS),
        Codec::Unsupported(_) => None,
    }
}

fn push_status_error(status: &mut DecoderStatus, error: StatusError) {
    status.errors.push(error);
    if status.errors.len() > 8 {
        status.errors.drain(..status.errors.len() - 8);
    }
}

#[cfg(test)]
mod transition_contract_tests {
    use super::*;
    use crate::status_protocol::StreamIdentity;
    use std::sync::Mutex;

    struct RecordingSink {
        spec: AudioSpec,
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl AudioSink for RecordingSink {
        fn write(&mut self, bytes: &[u8]) -> Result<()> {
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }

        fn specs(&self) -> AudioSpec {
            self.spec
        }
    }

    #[test]
    fn pcm_transition_clears_the_previous_decoded_format_atomically() {
        let capture = AudioSpec {
            format: AudioFormat::S16Le,
            rate: 48_000,
            layout: AudioLayout::Stereo,
        };
        let output = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        let reporter = StatusReporter::new(DecoderStatus::starting(
            "transition-status".to_owned(),
            transport_descriptor(capture),
            audio_descriptor(output),
            1,
        ));
        let mut detector = DetectionTracker::new(1, 1);

        publish_detection(&reporter, &detector.observe(Some(Codec::Ac3)));
        reporter.update(|status| {
            status.decoded = Some(AudioDescriptor {
                sample_rate: 48_000,
                sample_format: "f32(planar)".to_owned(),
                channels: 6,
                channel_layout: "5.1".to_owned(),
            });
        });
        publish_detection(&reporter, &detector.observe(None));

        let status = reporter.snapshot();
        assert_eq!(status.mode, DetectionMode::Pcm);
        assert_eq!(status.transport.framing, TransportFraming::Pcm);
        assert_eq!(status.codec, None);
        assert_eq!(status.decoded, None);
    }

    #[test]
    fn pcm_ac3_menu_dts_uses_one_output_and_silences_encoded_transition_bytes() {
        let capture = AudioSpec {
            format: AudioFormat::S16Le,
            rate: 48_000,
            layout: AudioLayout::Stereo,
        };
        let output = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut adaptive_sink: Option<Box<dyn AudioSink + Send>> = Some(Box::new(RecordingSink {
            spec: output,
            bytes: Arc::clone(&recorded),
        }));
        let mut decoder_sink = None;
        let normalizer = PcmNormalizer::new(capture, output).unwrap();
        let pcm_frame = [0xff, 0x7f, 0x00, 0x80];
        let encoded_carrier_frame = [0x72, 0xf8, 0x1f, 0x4e];

        write_for_mode(
            StableMode::Pcm,
            &pcm_frame,
            &normalizer,
            &mut adaptive_sink,
            &mut decoder_sink,
        )
        .unwrap();
        write_for_mode(
            StableMode::Encoded(Codec::Ac3),
            &encoded_carrier_frame,
            &normalizer,
            &mut adaptive_sink,
            &mut decoder_sink,
        )
        .unwrap();
        write_for_mode(
            StableMode::Pcm,
            &pcm_frame,
            &normalizer,
            &mut adaptive_sink,
            &mut decoder_sink,
        )
        .unwrap();
        write_for_mode(
            StableMode::Encoded(Codec::Dts),
            &encoded_carrier_frame,
            &normalizer,
            &mut adaptive_sink,
            &mut decoder_sink,
        )
        .unwrap();

        let identity = StreamIdentity::for_instance("transition-test");
        assert_eq!(
            identity.output_node_name,
            "open-cinema.decoder.transition-test.output"
        );
        let bytes = recorded.lock().unwrap();
        let output_frame_bytes = output.frame_bytes();
        assert_eq!(bytes.len(), output_frame_bytes * 4);
        let first_pcm = &bytes[..output_frame_bytes];
        let first_samples = first_pcm
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert!(first_samples[0] > 0.99);
        assert_eq!(first_samples[1], -1.0);
        assert_eq!(&first_samples[2..], &[0.0; 6]);
        assert!(
            bytes[output_frame_bytes..output_frame_bytes * 2]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert_eq!(
            &bytes[output_frame_bytes * 2..output_frame_bytes * 3],
            first_pcm
        );
        assert!(
            bytes[output_frame_bytes * 3..]
                .iter()
                .all(|byte| *byte == 0)
        );
        assert!(
            !bytes
                .windows(encoded_carrier_frame.len())
                .any(|window| { window == encoded_carrier_frame })
        );
    }
}
