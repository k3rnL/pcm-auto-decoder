use std::fmt::{format, Debug, Display};
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use anyhow::Context;
use libpulse_binding::channelmap::Map;
use libpulse_binding::channelmap::MapDef::{AIFF, ALSA};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

pub trait AudioSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
    fn specs(& self) -> Spec;
}

/* PulseAudio stereo sink */
pub(crate) struct PulseAudioSink {
    pa: Simple,
    spec: Spec,
}
impl PulseAudioSink {
    pub(crate) fn open(sink: Option<&str>, format: Format, rate: u32, channels: u8) -> anyhow::Result<Self> {
        let ss = Spec { format, rate, channels };
        anyhow::ensure!(ss.is_valid(), "Invalid sample spec");
        let mut cm = Map::default();
        cm.init_auto(channels, AIFF);
        let attr = BufferAttr {
            maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: u32::MAX,
        };
        let pa = Simple::new(
            None,
            "pcm-auto-decoder",
            Direction::Playback,
            sink,
            "PCM",
            &ss,
            Some(&cm),
            Some(&attr),
        )
            .context(format!("opening PulseAudio sink with spec={:?}", ss))?;
        Ok(Self { pa, spec: ss })
    }
}
impl AudioSink for PulseAudioSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.pa.write(bytes).context("pa_simple_write")
    }

    fn specs(& self) -> Spec {
        self.spec
    }
}

/* FIFO/file stereo sink */
pub(crate) struct FileSink {
    f: File,
    spec: Spec
}
impl FileSink {
    pub(crate) fn open(path: &PathBuf, format: Format, rate: u32, channels: u8) -> anyhow::Result<Self> {
        let f = File::options().read(true).write(true).open(path).context("open fifo_out")?;
        Ok(Self { f, spec: Spec {format, rate, channels} })
    }
}
impl AudioSink for FileSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.f.write_all(bytes).context("write fifo_out_pcm")
    }

    fn specs(& self) -> Spec {
        todo!()
    }
}