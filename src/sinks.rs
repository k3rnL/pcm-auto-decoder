use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use anyhow::Context;
use libpulse_binding::channelmap::Map;
use libpulse_binding::channelmap::MapDef::ALSA;
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

pub trait StereoPcmWriter {
    fn write_pcm_s16le_2ch(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
}
pub trait Pcm6chWriter {
    fn write_pcm_f32le_6ch(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
}

/* PulseAudio stereo sink */
pub(crate) struct PcmPaSink {
    pa: Simple,
}
impl PcmPaSink {
    pub(crate) fn open(sink: Option<&str>) -> anyhow::Result<Self> {
        let ss = Spec { format: Format::S16le, rate: 48_000, channels: 2 };
        anyhow::ensure!(ss.is_valid(), "Invalid sample spec");
        let mut cm = Map::default();
        cm.init_stereo();
        let attr = BufferAttr {
            maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: u32::MAX,
        };
        let pa = Simple::new(
            None,
            "pcm-auto-decoder",
            Direction::Playback,
            sink,
            "PCM stereo",
            &ss,
            Some(&cm),
            Some(&attr),
        )
            .context("opening PulseAudio stereo sink")?;
        Ok(Self { pa })
    }
}
impl StereoPcmWriter for PcmPaSink {
    fn write_pcm_s16le_2ch(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.pa.write(bytes).context("pa_simple_write stereo")
    }
}

/* PulseAudio 6ch sink */
pub(crate) struct Pa6Sink {
    pa: Simple,
}
impl Pa6Sink {
    pub(crate) fn open(sink: Option<&str>) -> anyhow::Result<Self> {
        let ss = Spec { format: Format::F32le, rate: 48_000, channels: 6 };
        anyhow::ensure!(ss.is_valid(), "Invalid 6ch sample spec");
        let mut cm = Map::default();
        cm.init_auto(6, ALSA);
        // cm.set_len(6);
        // cm.set_position(0, Position::FrontLeft);
        // cm.set_position(1, Position::FrontRight);
        // cm.set_position(2, Position::FrontCenter);
        // cm.set_position(3, Position::Lfe);
        // cm.set_position(4, Position::RearLeft);
        // cm.set_position(5, Position::RearRight);
        let attr = BufferAttr {
            maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: u32::MAX,
        };
        let pa = Simple::new(
            None,
            "pcm-auto-decoder",
            Direction::Playback,
            sink,
            "AC3 decode -> 5.1",
            &ss,
            Some(&cm),
            Some(&attr),
        )
            .context("opening PulseAudio 5.1 sink")?;
        Ok(Self { pa })
    }
}
impl Pcm6chWriter for Pa6Sink {
    fn write_pcm_f32le_6ch(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.pa.write(bytes).context("pa_simple_write 6ch")
    }
}

/* FIFO/file stereo sink */
pub(crate) struct PcmFileSink {
    f: File,
}
impl PcmFileSink {
    pub(crate) fn open(path: &PathBuf) -> anyhow::Result<Self> {
        let f = File::options().read(true).write(true).open(path).context("open fifo_out_pcm")?;
        Ok(Self { f })
    }
}
impl StereoPcmWriter for PcmFileSink {
    fn write_pcm_s16le_2ch(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.f.write_all(bytes).context("write fifo_out_pcm")
    }
}

/* FIFO/file 6ch sink */
pub(crate) struct File6Sink {
    f: File,
}
impl File6Sink {
    pub fn open(path: &PathBuf) -> anyhow::Result<Self> {
        let f = File::options().read(true).write(true).open(path).context("open fifo_out_6ch")?;
        Ok(Self { f })
    }
}
impl Pcm6chWriter for File6Sink {
    fn write_pcm_f32le_6ch(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.f.write_all(bytes).context("write fifo_out_6ch")
    }
}

/* AC-3 decoder using ffmpeg child: write IEC61937 in, read 6ch float out */
pub(crate) enum Ac3Out {
    Pa(Pa6Sink),
    File(File6Sink),
}
pub(crate) struct Ac3Sink {
    child_stdin: std::process::ChildStdin,
    _pump: thread::JoinHandle<anyhow::Result<()>>,
}
impl Ac3Sink {
    pub(crate) fn open(out: Ac3Out) -> anyhow::Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner", "-loglevel", "warning",
                "-f", "spdif", "-i", "pipe:0",
                "-f", "f32le", "-ac", "6", "-ar", "48000", "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg")?;

        let mut writer = out;
        let stdout = child.stdout.take().context("ffmpeg stdout")?;
        let pump = thread::spawn(move || -> anyhow::Result<()> {
            let mut reader = std::io::BufReader::new(stdout);
            let mut buf = vec![0u8; 6 * 1024 * 4];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                match &mut writer {
                    Ac3Out::Pa(pa6) => pa6.write_pcm_f32le_6ch(&buf[..n])?,
                    Ac3Out::File(f6) => f6.write_pcm_f32le_6ch(&buf[..n])?,
                }
            }
            Ok(())
        });

        Ok(Self { child_stdin: child.stdin.take().unwrap(), _pump: pump })
    }

    pub(crate) fn write_spdif(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.child_stdin.write_all(bytes).context("write IEC61937 to ffmpeg")
    }
}