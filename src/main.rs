use crate::sinks::AudioSink;
mod iec61937_detector;
mod sinks;
mod decoders;

use anyhow::{Context, Result};
use clap::Parser;
use libpulse_binding as pulse;
use libpulse_simple_binding::Simple;
use pulse::channelmap::Map;
use pulse::def::BufferAttr;
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;
use libpulse_binding::channelmap::MapDef::ALSA;
use crate::sinks::{FileSink, PulseAudioSink};
use iec61937_detector::Iec61937Detector;
use crate::decoders::{AudioDecoder, FfmpegDecoderSink};

/// IEC-61937 preamble words (big-endian)
const PA_SYNC: u16 = 0xF872;
const PB_SYNC: u16 = 0x4E1F;

const DEFAULT_CHUNK_FRAMES: usize = 2048;
const DEFAULT_DET_WINDOW_CHUNKS: usize = 64;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "PCM/AC3 autodetector/decoder: stdin FIFO or PulseAudio -> (PCM) -> PulseAudio or FIFO"
)]
struct Args {
    /// PulseAudio source name (ignored if --stdin is set)
    #[arg(long)]
    source: Option<String>,

    /// PulseAudio sink name (if neither --fifo-out-* set)
    #[arg(long)]
    sink: Option<String>,

    /// Read input from this file/FIFO instead of PulseAudio (expects S16LE 2ch @ 48kHz, may be IEC61937)
    #[arg(long)]
    stdin: Option<PathBuf>,

    /// Input channels, should always be 2 as it's the IEC61937 standard
    #[arg(long, default_value_t = 2)]
    in_channels: u8,

    /// Input rate, default 48kHz
    #[arg(long, default_value_t = 48_000)]
    in_rate: u32,

    /// Input format, default S16LE
    #[arg(long, default_value = "S16LE")]
    in_format: String,

    /// Write stereo PCM (S16LE 2ch @ 48kHz) here in PCM mode
    #[arg(long, value_name = "PATH")]
    fifo_out_pcm: Option<PathBuf>,

    /// Desired channels on the PCM output (when no compressed data is detected), default 2
    #[arg(long, default_value_t = 2)]
    out_pcm_channels: u8,

    /// Desired rate on the PCM output (when no compressed data is detected), default 48kHz
    #[arg(long, default_value_t = 48_000)]
    out_pcm_rate: u32,

    /// Desired format on the PCM output (when no compressed data is detected), default S16LE
    #[arg(long, default_value = "S16LE")]
    out_pcm_format: String,

    /// Write decoded 5.1 PCM (F32LE 6ch @ 48kHz) here in AC-3 mode
    #[arg(long, value_name = "PATH")]
    fifo_out_decoded: Option<PathBuf>,

    /// Desired channels on decoded output, default 6
    #[arg(long, default_value_t = 6)]
    out_decoded_channels: u8,

    /// Desired rate on decoded output, default 48kHz
    #[arg(long, default_value_t = 48_000)]
    out_decoded_rate: u32,

    /// Desired format on decoded output, default F32LE (float32le)
    #[arg(long, default_value = "F32LE")]
    out_decoded_format: String,

    /// Frames per read
    #[arg(long, default_value_t = DEFAULT_CHUNK_FRAMES)]
    chunk_frames: usize,

    /// Chunks without IEC-61937 before switching to PCM (and vice-versa)
    #[arg(long, default_value_t = DEFAULT_DET_WINDOW_CHUNKS)]
    det_window: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Unknown,
    Pcm,
    Iec61937,
}


/* --------------------- Input --------------------- */

enum Input {
    Pa(Simple, Vec<u8>),
    File(File, Vec<u8>),
}
impl Input {
    fn open(args: &Args) -> Result<Self> {
        let frames = args.chunk_frames;
        let bytes_per_frame = args.in_channels as usize /*ch*/ * 2 /*bytes*/;
        let buf = vec![0u8; frames * bytes_per_frame];

        if let Some(path) = &args.stdin {
            let f = File::options().read(true).open(path).context("open --stdin")?;
            Ok(Self::File(f, buf))
        } else {
            let source = args
                .source
                .as_ref()
                .map(|s| s.as_str())
                .context("--source is required when not using --stdin")?;
            let ss = Spec { format: Format::parse(&args.in_format), rate: args.in_rate, channels: args.in_channels };
            anyhow::ensure!(ss.is_valid(), "Invalid capture spec");
            let mut cm = Map::default();
            cm.init_auto(args.in_channels, ALSA);

            let attr = BufferAttr {
                maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX,
                fragsize: (frames * bytes_per_frame) as u32,
            };
            let pa_in = Simple::new(
                None,
                "pcm-auto-decoder",
                Direction::Record,
                Some(source),
                "capture",
                &ss,
                Some(&cm),
                Some(&attr),
            )
                .context("opening PulseAudio capture")?;
            Ok(Self::Pa(pa_in, buf))
        }
    }

    fn read_chunk(&mut self) -> Result<&[u8]> {
        match self {
            Input::Pa(pa, buf) => {
                pa.read(buf).context("pa_simple_read")?;
                Ok(buf.as_slice())
            }
            Input::File(f, buf) => {
                let mut got = 0usize;
                while got < buf.len() {
                    let n = f.read(&mut buf[got..])?;
                    if n == 0 {
                        // EOF
                        eprintln!("Input stream lost !");
                        sleep(Duration::from_millis(500));
                    }
                    got += n;
                }
                Ok(buf.as_slice())
            }
        }
    }
}

/* --------------------- Main --------------------- */

fn main() -> Result<()> {
    let args = Args::parse();

    // Declare sinks:
    let mut decoder_sink: Option<FfmpegDecoderSink> = None;

    // If FIFO outputs are set, we won't open PulseAudio sinks for those paths:
    let want_fifo_pcm = args.fifo_out_pcm.is_some();

    let mut pcm_sink: Option<Box<dyn AudioSink + Send>> = match &args.fifo_out_pcm {
        Some(p) => Some(Box::new(FileSink::open(p, Format::parse(&args.out_pcm_format), args.out_pcm_rate, args.out_pcm_channels)?)), // RDWR as above
        None => Some(Box::new(PulseAudioSink::open(args.sink.as_deref(), Format::parse(&args.out_pcm_format), args.out_pcm_rate, args.out_pcm_channels)?)),
    };

    let mut decoded_sink: Option<Box<dyn AudioSink + Send>> = match &args.fifo_out_decoded {
        Some(p) => Some(Box::new(FileSink::open(p, Format::parse(&args.out_decoded_format), args.out_decoded_rate, args.out_decoded_channels)?)),   // RDWR as above
        None => Some(Box::new(PulseAudioSink::open(args.sink.as_deref(), Format::parse(&args.out_decoded_format), args.out_decoded_rate, args.out_decoded_channels)?)),
    };

    // Prepare input (FIFO or PulseAudio)
    let mut input = Input::open(&args)?;

    let mut mode = Mode::Unknown;
    let mut chunks_without_61937 = 0usize;

    eprintln!(
        "Running… source={:?} stdin={:?} outPCM={:?} out6ch={:?} chunk_frames={} det_window={}",
        args.source, args.stdin, args.fifo_out_pcm, args.fifo_out_decoded, args.chunk_frames, args.det_window
    );

    loop {
        let chunk = input.read_chunk()?;
        let has_61937 = Iec61937Detector::find_preamble(chunk);

        match mode {
            Mode::Unknown => {
                if has_61937.is_some() {
                    eprintln!("[INIT] Found IEC-61937 (AC-3). Switching to AC-3 decode.");
                    mode = Mode::Iec61937;
                    chunks_without_61937 = 0;

                    // open AC3 sink target
                    decoder_sink = Some(FfmpegDecoderSink::wrap(decoded_sink.take().context("decoded_sink not set")?)?);

                    if let Some(s) = &mut decoder_sink {
                        s.write(chunk)?;
                    }
                } else {
                    chunks_without_61937 += 1;
                    if chunks_without_61937 >= args.det_window {
                        eprintln!("[INIT] Assuming PCM.");
                        mode = Mode::Pcm;

                        if let Some(s) = &mut pcm_sink {
                            s.write(chunk)?;
                        }
                    }
                }
            }
            Mode::Pcm => {
                if has_61937.is_some() {
                    eprintln!("Detected AC-3; switching PCM -> AC-3 decode.");

                    mode = Mode::Iec61937;
                    chunks_without_61937 = 0;

                    decoder_sink = Some(FfmpegDecoderSink::wrap(decoded_sink.take().context("decoded_sink not set")?)?);

                    if let Some(s) = &mut decoder_sink {
                        s.write(chunk)?;
                    }
                } else if let Some(s) = &mut pcm_sink {
                    s.write(chunk)?;
                }
            }
            Mode::Iec61937 => {
                if has_61937.is_some() {
                    chunks_without_61937 = 0;
                    if let Some(s) = &mut decoder_sink {
                        s.write(chunk)?;
                    }
                } else {
                    chunks_without_61937 += 1;
                    if chunks_without_61937 >= args.det_window {
                        eprintln!("Lost IEC-61937; switching to PCM.");

                        if let Some(dec) = decoder_sink.take() {
                            decoded_sink = Some(dec.finish()?)
                        }

                        decoder_sink = None;
                        mode = Mode::Pcm;

                        if let Some(s) = &mut pcm_sink {
                            s.write(chunk)?;
                        }
                    } else if let Some(s) = &mut decoder_sink {
                        // still push trailing words, helps decoder flush
                        s.write(chunk)?;
                    }
                }
            }
        }
    }
}