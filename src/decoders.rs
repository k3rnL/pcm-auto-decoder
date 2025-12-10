/* AC-3 decoder using ffmpeg child: write IEC61937 in, read 6ch float out */
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use anyhow::{anyhow, Context};
use ffmpeg_next::{codec, decoder, frame, ChannelLayout};
use ffmpeg_next::codec::{context, Id};
use ffmpeg_next::codec::traits::Decoder;
use ffmpeg_next::error::EAGAIN;
use ffmpeg_next::format::Sample;
use ffmpeg_next::format::sample::Type::Packed;
use libpulse_binding::sample::{Format, Spec};
use crate::iec61937_detector::{Iec61937Detector, Iec61937Preamble};
use crate::sinks::AudioSink;

pub trait AudioDecoder : AudioSink {
    fn wrap(sink: Box<dyn AudioSink + Send>) -> anyhow::Result<Self>
    where Self: Sized;

    fn finish(self) -> anyhow::Result<Box<dyn AudioSink + Send>>;
}

pub struct Ac3DecoderSink {
    decoder: decoder::Audio,
    resampler: Option<ffmpeg_next::software::resampling::Context>,

    frame: frame::Audio,
    pending: Vec<u8>,

    inner: Box<dyn AudioSink + Send>, // the wrapped sink

    out_spec: Spec,                       // what the user asked (format/channels/rate), spec of the inner sink
    out_sample_fmt: Sample,               // ffmpeg sample format matching out_spec.format
    out_layout: ChannelLayout,
    out_rate: u32,
}

impl Ac3DecoderSink {
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

    fn map_pa_format_to_ffmpeg_sample(fmt: Format) -> anyhow::Result<Sample> {
        use libpulse_binding::sample::Format as PF;

        Ok(match fmt {
            PF::S16NE | PF::S16le => Sample::I16(Packed),
            // PF::S24NE | PF::S24le => Sample::I24(Packed),
            PF::S32NE | PF::S32le => Sample::I32(Packed),
            PF::FLOAT32NE | PF::F32le => Sample::F32(Packed),
            // extend as you like; for now bail on weird ones:
            other => anyhow::bail!("Unsupported output Pulse format for decoder: {:?}", other),
        })
    }

    fn choose_layout_for_channels(ch: u8) -> anyhow::Result<ChannelLayout> {
        match ch {
            1 => Ok(ChannelLayout::MONO),
            2 => Ok(ChannelLayout::STEREO),
            6 => Ok(ChannelLayout::_5POINT1),
            _ => Err(anyhow::anyhow!("Unsupported channel count: {}", ch))
                // ChannelLayout::from_channels(ch as u32)
                // .ok_or_else(|| anyhow::anyhow!("Unsupported channel count: {}", ch))?,
        }
    }

    fn ensure_swr_initialized(&mut self) -> anyhow::Result<()> {
        if self.resampler.is_some() {
            return Ok(());
        }

        let in_fmt    = self.frame.format();
        let in_layout = self.frame.channel_layout();
        let in_rate   = self.frame.rate();

        let swr = ffmpeg_next::software::resampling::Context::get(
            in_fmt,
            in_layout,
            in_rate,
            self.out_sample_fmt,
            self.out_layout,
            self.out_rate,
        ).context("creating resampler context")?;

        self.resampler = Some(swr);
        Ok(())
    }

    /// Convert `self.frame` (decoder output) to bytes in the *requested* format.
    fn convert_and_pack_current_frame(&mut self) -> anyhow::Result<Vec<u8>> {
        let swr = self.resampler.as_mut().expect("resampler must be initialized");
        let mut out_frame = frame::Audio::empty();

        swr.run(&self.frame, &mut out_frame)
            .context("swresample run")?;

        Self::frame_to_interleaved_bytes(&out_frame, self.out_sample_fmt)
    }

    fn frame_to_interleaved_bytes(
        frame: &frame::Audio,
        fmt: Sample,
    ) -> anyhow::Result<Vec<u8>> {
        let ch  = frame.channels() as usize;
        let n   = frame.samples() as usize;

        Ok(match fmt {
            Sample::I16(Packed) => {
                // interleaved i16: just copy
                let data = frame.data(0);
                data[..(n * ch * 2)].to_vec()
            }
            // Sample::I24(ST::Packed) => {
            //     let data = frame.data(0);
            //     data[..(n * ch * 3)].to_vec()
            // }
            Sample::I32(Packed) => {
                let data = frame.data(0);
                data[..(n * ch * 4)].to_vec()
            }
            Sample::F32(Packed) => {
                let data = frame.data(0);
                data[..(n * ch * 4)].to_vec()
            }
            other => anyhow::bail!("Unsupported packed sample format in frame_to_interleaved_bytes: {:?}", other),
        })
    }

    pub fn take_ac3_burst(stash: &mut Vec<u8>) -> Option<(Iec61937Preamble, Vec<u8>)> {
        // Need at least preamble
        if stash.len() < 8 {
            return None;
        }


        // Try to find preamble in current buffer
        let (idx, preamble) = match Iec61937Detector::find_preamble_with_index(&stash[..]) {
            Some(t) => t,
            None => {
                // No preamble yet: drop everything except the last 7 bytes
                // so we don't miss a preamble split across chunks
                let keep_from = stash.len().saturating_sub(7);
                if keep_from > 0 {
                    stash.drain(..keep_from);
                }
                return None;
            }
        };

        // Optional: filter by stream-type = AC-3 (usually data-type bits 0..4 == 1)
        // Adjust the match to your enum name:
        // if preamble.stream_type != StreamType::Ac3 { ... }
        //
        // For now, assume AC-3 data-type value is 1:
        // (if stream_type is a u8, adapt accordingly)
        // if preamble.stream_type != 1.into() { ... }

        // Burst payload begins right after the 4 preamble words (8 bytes total)
        let payload_start = idx + 8;

        // For AC-3, Pd is in bits; for E-AC-3, Pd is already bytes.
        let payload_len = match preamble.payload_bytes() {
            Some(n) => n,
            None => {
                // Unsupported or unknown stream type; keep data after idx and wait for next burst
                stash.drain(..idx);
                return None;
            }
        };

        // Check we have the full burst in the buffer
        let payload_end = match payload_start.checked_add(payload_len) {
            Some(end) if end <= stash.len() => end,
            _ => {
                // Not enough data yet: keep everything from idx onwards
                // (in case the preamble started near the end)
                stash.drain(..idx);
                return None;
            }
        };

        // Extract payload
        let payload = stash[payload_start..payload_end].to_vec();

        // Drop everything up to the end of this burst
        stash.drain(..payload_end);

        Some((preamble, payload))
    }

    fn iec61937_payload_to_ac3_be(payload: &[u8], fmt: Format) -> Option<Vec<u8>> {
        // 1) First, rebuild the AC-3 bitstream as a sequence of 16-bit *words*
        //    in big-endian byte order.
        let mut be_words: Vec<u8> = match fmt {
            // Most common: 16-bit IEC words captured as S16_LE
            Format::S16le | Format::S16NE => {
                if payload.len() < 2 {
                    return None;
                }
                let mut out = Vec::with_capacity(payload.len());
                for chunk in payload.chunks_exact(2) {
                    let w = u16::from_le_bytes([chunk[0], chunk[1]]);
                    out.extend_from_slice(&w.to_be_bytes());
                }
                out
            }

            // Sometimes drivers expose the S/PDIF stream as S32_LE.
            // Typically the IEC 16-bit word is in the high 16 bits of the 32-bit slot.
            // If in your hardware it's in the low 16 bits, replace (s >> 16) with (s & 0xFFFF).
            Format::S32le | Format::S32NE => {
                if payload.len() < 4 {
                    return None;
                }
                let mut out = Vec::with_capacity(payload.len() / 2);
                for chunk in payload.chunks_exact(4) {
                    let s = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    // assume IEC word is in the HIGH 16 bits:
                    let w = (s >> 16) as u16;
                    out.extend_from_slice(&w.to_be_bytes());
                }
                out
            }

            // Other PCM formats not supported for IEC61937 here
            other => {
                eprintln!("iec61937_payload_to_ac3_be: unsupported capture format {:?}", other);
                return None;
            }
        };

        // 2) Find AC-3 syncword 0x0B77 in the reconstructed bitstream
        let mut sync_idx = None;
        for i in 0..be_words.len().saturating_sub(1) {
            if be_words[i] == 0x0B && be_words[i + 1] == 0x77 {
                sync_idx = Some(i);
                break;
            }
        }
        let sync_idx = sync_idx?;

        // 3) Return from syncword onward (you can later trim to exact frame size)
        Some(be_words.split_off(sync_idx))
    }
}

impl AudioSink for Ac3DecoderSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        // 1. Accumulate raw AC-3 bytes
        self.pending.extend_from_slice(bytes);

        // 2. While we have at least one whole frame, decode it
        while let Some((_, frame_bytes)) = Self::take_ac3_burst(&mut self.pending) {
            if let Some(raw_ac3) = Self::iec61937_payload_to_ac3_be(&frame_bytes, self.specs().format) {
                // eprintln!(
                //     "AC3 packet len={} first 16 bytes: {:02X?}",
                //     raw_ac3.len(),
                //     &raw_ac3[..raw_ac3.len().min(16)]
                // );

                // {
                //     // Debug dump – remove later
                //     let mut f = OpenOptions::new()
                //         .create(true)
                //         .append(true)
                //         .open("/tmp/debug.ac3")?;
                //     f.write_all(&raw_ac3)?;
                // }

                // Send to decoder
                let packet = codec::packet::Packet::copy(&raw_ac3);
                // if let Err(e) = self.decoder.send_packet(&packet) {
                //     // If it's just EOF, stop; otherwise, propagate
                //     if e != ffmpeg_next::Error::Eof {
                //         return Err(anyhow::anyhow!("send_packet failed: {e}"));
                //     }
                // }
                if let Err(e) = self.decoder.send_packet(&packet) {
                    eprintln!("AC3: send_packet failed for this burst: {e}, dropping it");
                    // Just skip this burst and keep going; decoder will sync later if data becomes valid
                    continue;
                }

                // Pull all available frames
                loop {
                    match self.decoder.receive_frame(&mut self.frame) {
                        Ok(_) => {
                            self.ensure_swr_initialized()?;
                            let buf = self.convert_and_pack_current_frame()?;
                            self.inner.write(&buf)?;
                        }
                        Err(ffmpeg_next::Error::Other { errno }) if errno == EAGAIN => break, // needs more packets
                        Err(ffmpeg_next::Error::Eof) => break,   // flushed
                        Err(e) => return Err(anyhow::anyhow!("receive_frame failed: {e}")),
                    }
                }
            } else {
                eprintln!("No AC-3 syncword found in burst, dropping");
            }
        }

        Ok(())
    }

    fn specs(& self) -> Spec {
        self.out_spec
    }
}

impl AudioDecoder for Ac3DecoderSink {

    fn wrap(sink: Box<dyn AudioSink + Send>) -> anyhow::Result<Self> {
        let out_spec = sink.specs();
        let out_sample_fmt = Self::map_pa_format_to_ffmpeg_sample(out_spec.format)?;
        let out_layout     = Self::choose_layout_for_channels(out_spec.channels)?;
        let out_rate       = out_spec.rate;

        // Find AC3 decoder
        let ac3 = decoder::find(Id::AC3)
            .ok_or_else(|| anyhow::anyhow!("AC3 decoder not found in ffmpeg"))?;

        // Create codec context
        let mut decoder = context::Context::new_with_codec(ac3)
            .decoder().audio()?;

        // Optionally set requested sample_fmt / channels here, or resample later.
        // For simplicity, let ffmpeg give us its natural format (often FLTP/F32).

        Ok(Self {
            decoder,
            resampler: None,
            frame: frame::Audio::empty(),
            pending: Vec::with_capacity(4096),
            inner: sink,
            out_spec,
            out_sample_fmt,
            out_layout,
            out_rate,
        })
    }

    /// Close ffmpeg and release sink
    fn finish(mut self) -> anyhow::Result<Box<dyn AudioSink + Send>> {
        // Close ffmpeg's stdin so it can flush and exit.
        drop(self.decoder);
        drop(self.resampler.take());

        Ok(self.inner)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iec61937_detector::StreamType::Ac3;
    use anyhow::Context;
    use base64::Engine;

    #[test]
    fn can_take_ac3_frame() -> Result<(), String> {
        use base64::prelude::*;

        let data = "cvgfTgEAAFB3Cy/EQCSEL8Ir+wFeh+cbn++uPvP5z+e+of8ywdx7ik+c2JTYbyuIq+IY5D1zdQaI2Q+PL6YsxSS8F57bmIWYcRobLiMwSRZpP/t2IgJAygnO2wRzVZkgwR4DhBIMUKsB0qMuUkqx8SE+gufC6KBGwfdjk4IkwI5hxINUwlDgxUGeYyQickHwwQBD/EIZQCZB0kO7wj0AXQGxw4wCYICKQZTDY0J5QK3BeMM+wo6AzkFmwyECoEDoQVQDCMOwgAOBlQPpAnmAKoF8A8eClIBQgWUDoIKhgGwBVIOEgrOAiAFEA2mCvwCfgTUDU4LQgLmBKoM4AtIAx4EgAyqC4IDdgR2DHoLrgO0BFgMUAvgA/AESA/gC6wAIAQUA6AICAyIB8wDKAgsDPQHwALsCGANPAeIArgI3A3kB4ACLAiMDdAHOAHsCMQOLAckAZwIwA5YBxQBgAkADrQHHAFQCQAOzAb8ASQJBA7EBqwA9AlUDygGmACgCUgPXAagAIAJVA+IBqQAcAlkD5AGeABICYwP1AaEADgJoA/0BnAABAmID9wGRAAIDegMPARgA1gLaAx4BFgDaAvIDOAEYAdICAANKAQwAuAL0AkQB+gCoAvoBUD8cAyDxXnz8tfpUej6rraN09dOpfiW6lq+ppZ2lqOjrK2i2pvuCKve41P2QnfJrc3Ol9L3PJyS4CX1nVXXzjEMArACIDHAAeA0aQXWr1pDIe9YhGJau0qHE3rFBQLNAiAIEAIQOgAQAtADwA0AEwAFADoAEABYAGAOQBCAYgJAA6ABgAKADgAOAAwAFAAgALAAEABYADAAAALAB8ABgAwAA4APgAyABAAMAA0ADgAHgAQAAQAMwAYAAIAKAA8ABgACAAcAAYAGAAUADAACAAKAAQANAAgADwALAA8ALgAmADwAOAAeABoAOAATABAAFAA+ACoAEAAIACgAMAAAAAwAJAAEACwAEgA0ADAAOAAwAAAAAgAEAA0ALAAMACwACgAQADQAKAAEACgABAAqADkAMAA0AD4AMAAmADQAPAASABAANwA8ADwABgAQAA4ANAA6AC0APYAuACwAAAAkACwAJAA0ABAAAgAQAAIAAAAUABAADAAYADAAAAAsABAAOAAwACgAFgANABoAGAA4AAIAIgA2ABAAPgAMADYAFgA+ABAAMAAuAAAADBuCBGmhAAw48UIELLIgBAx8AYAMG8IAACSCABXyAATawwAcA8AACgAIE6AEDBgDiwP0A+gD/8K9U/jXb4P4Tta8B6YHIAYy1JgAr/zT9UBRAEABAAEAOABEAAAC4AAAAKA0gCkAGAAgAAAMAA4AAAAEAOgA0ABwDMALAA8AB4A==";
        let mut bytes = BASE64_STANDARD
            .decode(data)
            .map_err(move |e| e.to_string())?;

        let (index, preamble) = match Iec61937Detector::find_preamble_with_index(bytes.as_slice()) {
            Some((index, preamble)) => (index, preamble),
            None => return Err("Did not find preamble in the data".to_string()),
        };
        assert_eq!(preamble.stream_type, Ac3);
        println!("index={}", index);

        let size_before = bytes.len();

        let (_, burst) = Ac3DecoderSink::take_ac3_burst(bytes.as_mut()).unwrap(); // !!! Fails because the data is smaller than ac3 packet size
        assert_ne!(size_before, bytes.len());
        assert_eq!(size_before - burst.len(), bytes.len());

        Ok(())
    }
}