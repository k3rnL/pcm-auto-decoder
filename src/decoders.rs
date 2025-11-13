/* AC-3 decoder using ffmpeg child: write IEC61937 in, read 6ch float out */
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use anyhow::{anyhow, Context};
use libpulse_binding::sample::{Format, Spec};
use crate::sinks::AudioSink;

pub trait AudioDecoder : AudioSink {
    fn wrap(sink: Box<dyn AudioSink + Send>) -> anyhow::Result<Self>
    where Self: Sized;

    fn finish(self) -> anyhow::Result<Box<dyn AudioSink + Send>>;
}

pub struct FfmpegDecoderSink {
    child_stdin: std::process::ChildStdin,
    child: Option<Child>,
    _pump: Option<thread::JoinHandle<anyhow::Result<Box<dyn AudioSink + Send>>>>,
    specs: Spec
}

impl FfmpegDecoderSink {
    fn bytes_per_sample(format: Format) -> usize {
        use Format::*;
        match format {

            S16le | S16be => 2,
            S24le | S24be => 3,
            S32le | S32be => 4,
            F32le | F32be => 4,
            // extend if you want other formats
            _ => panic!("Unsupported format for decoder"),
        }
    }
}

impl AudioSink for FfmpegDecoderSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.child_stdin.write_all(bytes).context("write IEC61937 to ffmpeg")
    }

    fn specs(& self) -> Spec {
        self.specs
    }
}

impl AudioDecoder for FfmpegDecoderSink {

    fn wrap(sink: Box<dyn AudioSink + Send>) -> anyhow::Result<Self>
    {
        let spec = sink.specs();
        let chans = spec.channels as usize;
        let sample_bytes = FfmpegDecoderSink::bytes_per_sample(spec.format);
        let frame_bytes = chans * sample_bytes;
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner", "-loglevel", "warning",
                "-f", "spdif", "-i", "pipe:0",
                "-f", &spec.format.to_string().unwrap(), "-ac", &spec.channels.to_string(), "-ar", &spec.rate.to_string(), "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg")?;

        let writer = sink;
        let stdout = child.stdout.take().context("ffmpeg stdout")?;
        let pump = thread::spawn(move || -> anyhow::Result<Box<dyn AudioSink + Send>> {
            let mut reader = std::io::BufReader::new(stdout);
            let mut inbuf = vec![0u8; 8 * 1024];
            const FRAME_BYTES: usize = 6 /*ch*/ * 4 /*f32*/;

            let mut stash: Vec<u8> = Vec::with_capacity(128 * frame_bytes);
            let mut out: Option<Box<dyn AudioSink + Send>> = Some(writer);

            loop {
                let n = reader.read(&mut inbuf)?;
                if n == 0 { break; }

                // accumulate
                stash.extend_from_slice(&inbuf[..n]);

                // number of bytes we can safely write (multiple of frame size)
                let aligned = stash.len() - (stash.len() % frame_bytes);
                if aligned > 0 {
                    if let Some(w) = out.as_mut() {
                        if let Err(e) = w.write(&stash[..aligned]) {
                            eprintln!("sink write failed: {e}; dropping samples to keep decoder alive");
                            out = None;
                        }
                    }
                    stash.drain(..aligned);
                }
            }

            // optional: flush any tail by padding to a frame boundary
            if !stash.is_empty() {
                let pad = FRAME_BYTES - (stash.len() % FRAME_BYTES);
                if pad < FRAME_BYTES { stash.extend(std::iter::repeat(0u8).take(pad)); }
                if let Some(w) = out.as_mut() {
                    let _ = w.write(&stash); // ignore final error
                }
            }

            Ok(out.unwrap())
        });

        Ok(Self { child_stdin: child.stdin.take().context("ffmpeg stdin")?, child: Some(child), _pump: Some(pump), specs: spec })
    }

    /// Close ffmpeg input, wait for it to exit, join the pump thread
    /// and return the original sink so it can be reused.
    fn finish(mut self) -> anyhow::Result<Box<dyn AudioSink + Send>> {
        // Close ffmpeg's stdin so it can flush and exit.
        drop(self.child_stdin);

        if let Some(mut child) = self.child.take() {
            let _ = child.wait(); // ignore exit code here or handle it if you want
        }

        let handle = self._pump.take().ok_or_else(|| anyhow!("pump missing"))?;
        let sink = handle
            .join()
            .map_err(|_| anyhow!("pump thread panicked"))??;

        Ok(sink)
    }

}