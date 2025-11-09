mod iec61937_detector;
mod sinks;

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
use crate::sinks::{File6Sink, StereoPcmWriter};
use iec61937_detector::Iec61937Detector;
use sinks::{Ac3Out, Ac3Sink, Pa6Sink, PcmFileSink, PcmPaSink};

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

    /// Write stereo PCM (S16LE 2ch @ 48kHz) here in PCM mode
    #[arg(long, value_name = "PATH")]
    fifo_out_pcm: Option<PathBuf>,

    /// Write decoded 5.1 PCM (F32LE 6ch @ 48kHz) here in AC-3 mode
    #[arg(long, value_name = "PATH")]
    fifo_out_6ch: Option<PathBuf>,

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
        let bytes_per_frame = 2 /*ch*/ * 2 /*bytes*/;
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
            // PulseAudio capture: S16LE 2ch @ 48kHz
            let ss = Spec { format: Format::S16le, rate: 48_000, channels: 2 };
            anyhow::ensure!(ss.is_valid(), "Invalid capture spec");
            let mut cm = Map::default();
            cm.init_stereo();

            let mut attr = BufferAttr::default();
            attr.fragsize = (frames * bytes_per_frame) as u32;
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

    fn read_chunk<'a>(&'a mut self) -> Result<&'a [u8]> {
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

    // Choose sinks:
    let mut pcm_sink_pa: Option<PcmPaSink> = None;
    let mut ac3_sink: Option<Ac3Sink> = None;

    // If FIFO outputs are set, we won't open PulseAudio sinks for those paths:
    let want_fifo_pcm = args.fifo_out_pcm.is_some();
    let want_fifo_6ch = args.fifo_out_6ch.is_some();

    let mut pcm_sink_file = match &args.fifo_out_pcm {
        Some(p) => Some(PcmFileSink::open(p)?), // RDWR as above
        None => None,
    };

    let mut file6_sink = match &args.fifo_out_6ch {
        Some(p) => Some(File6Sink::open(p)?),   // RDWR as above
        None => None,
    };

    // Prepare input (FIFO or PulseAudio)
    let mut input = Input::open(&args)?;

    let mut mode = Mode::Unknown;
    let mut chunks_without_61937 = 0usize;
    let mut detector = Iec61937Detector::new();

    eprintln!(
        "Running… source={:?} stdin={:?} outPCM={:?} out6ch={:?} chunk_frames={} det_window={}",
        args.source, args.stdin, args.fifo_out_pcm, args.fifo_out_6ch, args.chunk_frames, args.det_window
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
                    if want_fifo_6ch {
                        let f6 = File6Sink::open(args.fifo_out_6ch.as_ref().unwrap())?;
                        ac3_sink = Some(Ac3Sink::open(Ac3Out::File(f6))?);
                    } else {
                        let pa6 = Pa6Sink::open(args.sink.as_deref())?;
                        ac3_sink = Some(Ac3Sink::open(Ac3Out::Pa(pa6))?);
                    }
                    if let Some(s) = &mut ac3_sink {
                        s.write_spdif(chunk)?;
                    }
                } else {
                    chunks_without_61937 += 1;
                    if chunks_without_61937 >= args.det_window {
                        eprintln!("[INIT] Assuming PCM.");
                        mode = Mode::Pcm;

                        if !want_fifo_pcm {
                            pcm_sink_pa = Some(PcmPaSink::open(args.sink.as_deref())?);
                        }
                        if let Some(s) = &mut pcm_sink_file {
                            s.write_pcm_s16le_2ch(chunk)?;
                        }
                        if let Some(s) = &mut pcm_sink_pa {
                            s.write_pcm_s16le_2ch(chunk)?;
                        }
                    }
                }
            }
            Mode::Pcm => {
                if has_61937.is_some() {
                    eprintln!("Detected AC-3; switching PCM -> AC-3 decode.");
                    pcm_sink_file = None;
                    pcm_sink_pa = None;

                    mode = Mode::Iec61937;
                    chunks_without_61937 = 0;

                    if want_fifo_6ch {
                        let f6 = File6Sink::open(args.fifo_out_6ch.as_ref().unwrap())?;
                        ac3_sink = Some(Ac3Sink::open(Ac3Out::File(f6))?);
                    } else {
                        let pa6 = Pa6Sink::open(args.sink.as_deref())?;
                        ac3_sink = Some(Ac3Sink::open(Ac3Out::Pa(pa6))?);
                    }
                    if let Some(s) = &mut ac3_sink {
                        s.write_spdif(chunk)?;
                    }
                } else {
                    if let Some(s) = &mut pcm_sink_file {
                        s.write_pcm_s16le_2ch(chunk)?;
                    }
                    if let Some(s) = &mut pcm_sink_pa {
                        s.write_pcm_s16le_2ch(chunk)?;
                    }
                }
            }
            Mode::Iec61937 => {
                if has_61937.is_some() {
                    chunks_without_61937 = 0;
                    if let Some(s) = &mut ac3_sink {
                        s.write_spdif(chunk)?;
                    }
                } else {
                    chunks_without_61937 += 1;
                    if chunks_without_61937 >= args.det_window {
                        eprintln!("Lost IEC-61937; switching to PCM.");
                        ac3_sink = None;
                        mode = Mode::Pcm;

                        if want_fifo_pcm {
                            pcm_sink_file = Some(PcmFileSink::open(args.fifo_out_pcm.as_ref().unwrap())?);
                        } else {
                            pcm_sink_pa = Some(PcmPaSink::open(args.sink.as_deref())?);
                        }
                        if let Some(s) = &mut pcm_sink_file {
                            s.write_pcm_s16le_2ch(chunk)?;
                        }
                        if let Some(s) = &mut pcm_sink_pa {
                            s.write_pcm_s16le_2ch(chunk)?;
                        }
                    } else if let Some(s) = &mut ac3_sink {
                        // still push trailing words, helps decoder flush
                        s.write_spdif(chunk)?;
                    }
                }
            }
        }
    }
}