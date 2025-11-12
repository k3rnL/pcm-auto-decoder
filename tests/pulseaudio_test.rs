use std::{
    fs,
    io,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use std::fs::OpenOptions;
use std::io::Read;
use anyhow::{Context, Error, Result};
use assert_cmd::cargo;

/// Expected audio parameters
const SR: u32 = 48_000;
const DURATION_S: u32 = 1;
const NCH: usize = 6;
const BYTES_PER_SAMPLE: usize = 4; // float32LE
const EXPECTED_BYTES: usize = (SR as usize) * (DURATION_S as usize) * NCH * BYTES_PER_SAMPLE;

fn kill_silently(p: &mut Child) {
    let _ = p.kill();
    let _ = p.wait();
}

fn wait_for_size(path: &Path, at_least: usize, timeout: Duration) -> io::Result<usize> {
    let start = Instant::now();
    loop {
        if path.exists() {
            let md = fs::metadata(path)?;
            let sz = md.len() as usize;
            if sz >= at_least {
                return Ok(sz);
            }
        }
        if start.elapsed() > timeout {
            let sz = if path.exists() { fs::metadata(path)?.len() as usize } else { 0 };
            return Ok(sz); // return whatever we got; caller can assert
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn decode_ac3_pa() -> Result<()> {
    // Fresh pipes in /tmp to avoid stale files from previous runs
    let pa_in = Path::new("/tmp/pa.input");
    let pa_out = Path::new("/tmp/pa.output");
    let ref_out = Path::new("/tmp/ref.f32le");

    let _ = fs::remove_file(pa_in);
    let _ = fs::remove_file(pa_out);
    let _ = fs::remove_file(ref_out);

    // 1) Start PulseAudio with:
    //    - a pipe *source* that reads S/PDIF (AC3-in-PCM) from /tmp/pa.input (2ch S16LE @ 48k)
    //    - a pipe *sink* that writes decoded PCM to /tmp/pa.output (6ch float32LE @ 48k)
    //    - native protocol so your binary can connect
    let mut pulseaudio = Command::new("pulseaudio")
        .args([
            "-n",
            "-L", "module-pipe-source file=/tmp/pa.input rate=48000 format=S16LE channels=2",
            "-L", "module-pipe-sink file=/tmp/pa.output rate=48000 format=float32LE channels=6",
            "-L", "module-native-protocol-unix",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start pulseaudio daemon")?;

    // Give PA a brief moment to load modules and create the pipes
    thread::sleep(Duration::from_millis(200));

    // 2) Start your decoder binary that should auto-detect AC-3 over S/PDIF
    //    from the PulseAudio source and write decoded 5.1 float32LE to PA sink.
    let mut binary = Command::new(cargo::cargo_bin!("pcm-auto-decoder"))
        .args([
            "--source", "fifo_input",
            "--sink", "fifo_output",
            "--chunk-frames", "256",
            "--det-window", "12",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start pcm-auto-decoder")?;

    // 3) Start ffmpeg AC-3 S/PDIF generator:
    //    Create a 5.1 AC-3 bitstream (640k) carried over S/PDIF into /tmp/pa.input.
    //    We duplicate a 880 Hz tone into all 6 channels so we have a clean deterministic reference.
    //
    //    Note: Although S/PDIF is 2ch 16-bit in the *container*, the AC-3 payload is 5.1.
    let pan_51 = "pan=5.1(side)|FL<c0|FR<c0|FC<c0|LFE<c0|SL<c0|SR<c0";
    let mut ffmpeg_generator = Command::new("ffmpeg")
        .args([
            "-y",
            "-re", // real-time; makes the pipeline behave like actual playback
            "-f", "lavfi",
            "-i", "sine=frequency=880:sample_rate=48000:duration=1000",
            // "-filter_complex", pan_51,
            "-ar", "48000",
            "-ac", "6",
            "-c:a", "ac3",
            "-b:a", "640k",
            "-f", "spdif",
            "/tmp/pa.input",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start ffmpeg ac3 spdif generator")?;

    // // 4) Wait until /tmp/pa.output reaches at least the expected size
    // let got = wait_for_size(pa_out, EXPECTED_BYTES, Duration::from_secs(15))
    //     .context("waiting for decoded output")?;
    // assert!(
    //     got >= EXPECTED_BYTES,
    //     "decoded output too small: got {got} bytes, expected at least {EXPECTED_BYTES}"
    // );

    // // Trim any trailing bytes (e.g., if the pipeline wrote a bit more)
    // {
    //     use std::os::unix::fs::FileExt;
    //     use std::fs::OpenOptions;
    //
    //     let mut f = OpenOptions::new().read(true).write(true).open(pa_out)?;
    //     if got > EXPECTED_BYTES {
    //         f.set_len(EXPECTED_BYTES as u64)?;
    //     }
    // }

    println!("Started to read");

    // Read the fifo
    let mut f = OpenOptions::new().read(true).write(true).open("/tmp/pa.output")?;
    let mut buf = vec![0u8; EXPECTED_BYTES];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        let n = f.read(&mut buf)?;
        assert_ne!(n, 0, "FIFO closed too early");

        if buf.iter().any(|&x| x != 0) {
            kill_silently(&mut ffmpeg_generator);
            kill_silently(&mut binary);
            kill_silently(&mut pulseaudio);
            return Ok(());
        }
    }

    kill_silently(&mut ffmpeg_generator);
    kill_silently(&mut binary);
    kill_silently(&mut pulseaudio);
    return Err(Error::msg("Never found decoded data in the pipe"));

    println!("Finsihed to read");

    // Stop processes now that we have enough data
    kill_silently(&mut ffmpeg_generator);
    kill_silently(&mut binary);
    kill_silently(&mut pulseaudio);

    // 5) Build the reference 5.1 float32LE file directly (no AC-3)
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f", "lavfi",
            "-i", "sine=frequency=880:sample_rate=48000:duration=1",
            "-filter_complex", pan_51,
            "-ar", "48000",
            "-ac", "6",
            "-f", "f32le",
            ref_out.to_str().unwrap(),
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("ffmpeg reference generation failed to run")?;
    assert!(status.success(), "ffmpeg reference generation failed");

    // 6) Byte-for-byte comparison
    // let got_bytes = fs::read(pa_out).context("read decoded /tmp/pa.output")?;
    let mut got_bytes = buf;
    let mut ref_bytes = fs::read(ref_out).context("read reference /tmp/ref.f32le")?;

    got_bytes.truncate(512);
    ref_bytes.truncate(512);

    assert_eq!(
        got_bytes.len(),
        ref_bytes.len(),
        "size mismatch: decoded {} vs ref {} bytes",
        got_bytes.len(),
        ref_bytes.len()
    );
    assert_eq!(
        &got_bytes[..],
        &ref_bytes[..],
        "decoded output does not match reference byte-for-byte"
    );

    // Cleanup
    // let _ = fs::remove_file(pa_in);
    // let _ = fs::remove_file(pa_out);
    // let _ = fs::remove_file(ref_out);

    Ok(())
}