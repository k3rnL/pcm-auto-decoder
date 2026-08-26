/* AC-3/E-AC3/DTS decoder using ffmpeg: write IEC61937 in, read multi-channel audio out */
use crate::iec61937_detector::{Iec61937Detector, Iec61937Preamble, StreamType};
use crate::sinks::{AudioChannel, AudioFormat, AudioLayout, AudioSink, AudioSpec};
use crate::status_protocol::AudioDescriptor;
use anyhow::Context;
use ffmpeg_next::codec::{Id, context};
use ffmpeg_next::error::EAGAIN;
use ffmpeg_next::format::Sample;
use ffmpeg_next::format::sample::Type::Packed;
use ffmpeg_next::{ChannelLayout, codec, decoder, frame};

pub type DecodedFrameObserver = Box<dyn FnMut(AudioDescriptor) + Send>;

pub struct Ac3DecoderSink {
    decoder: decoder::Audio,
    codec_id: Id, // track which codec we're using
    resampler: Option<ffmpeg_next::software::resampling::Context>,

    frame: frame::Audio,
    pending: Vec<u8>,

    inner: Box<dyn AudioSink + Send>, // the wrapped sink

    carrier_format: AudioFormat,
    out_spec: AudioSpec,
    out_sample_fmt: Sample, // ffmpeg sample format matching out_spec.format
    out_layout: ChannelLayout,
    out_rate: u32,
    decoded_frame_observer: Option<DecodedFrameObserver>,
    last_decoded_descriptor: Option<AudioDescriptor>,
    resampler_input: Option<(String, u64, u32)>,
}

impl Ac3DecoderSink {
    fn map_output_format_to_ffmpeg_sample(fmt: AudioFormat) -> anyhow::Result<Sample> {
        Ok(match fmt {
            AudioFormat::S16Le => Sample::I16(Packed),
            AudioFormat::S32Le => Sample::I32(Packed),
            AudioFormat::F32Le => Sample::F32(Packed),
        })
    }

    fn ffmpeg_layout(layout: AudioLayout) -> ChannelLayout {
        match layout {
            AudioLayout::Mono => ChannelLayout::MONO,
            AudioLayout::Stereo => ChannelLayout::STEREO,
            AudioLayout::Surround51Side => ChannelLayout::_5POINT1,
            AudioLayout::Surround51Rear => ChannelLayout::_5POINT1_BACK,
            AudioLayout::Surround71 => ChannelLayout::_7POINT1,
        }
    }

    fn ensure_swr_initialized(&mut self) -> anyhow::Result<()> {
        let input_signature = (
            format!("{:?}", self.frame.format()),
            self.frame.channel_layout().bits(),
            self.frame.rate(),
        );
        if self.resampler_input.as_ref() == Some(&input_signature) {
            return Ok(());
        }

        let in_fmt = self.frame.format();
        let in_layout = self.frame.channel_layout();
        let in_rate = self.frame.rate();

        eprintln!(
            "Resampler initialized: decoded format={:?}, layout={:?}, rate={} Hz -> output format={:?}, layout={:?}, rate={} Hz",
            in_fmt, in_layout, in_rate, self.out_sample_fmt, self.out_layout, self.out_rate
        );

        let swr = ffmpeg_next::software::resampling::Context::get(
            in_fmt,
            in_layout,
            in_rate,
            self.out_sample_fmt,
            self.out_layout,
            self.out_rate,
        )
        .context("creating resampler context")?;

        self.resampler = Some(swr);
        self.resampler_input = Some(input_signature);
        Ok(())
    }

    fn observe_decoded_frame(&mut self) {
        let descriptor = AudioDescriptor {
            sample_rate: self.frame.rate(),
            sample_format: format!("{:?}", self.frame.format()).to_ascii_lowercase(),
            channels: self.frame.channels(),
            channel_layout: Self::channel_layout_name(
                self.frame.channel_layout(),
                self.frame.channels(),
            ),
        };
        if self.last_decoded_descriptor.as_ref() == Some(&descriptor) {
            return;
        }
        self.last_decoded_descriptor = Some(descriptor.clone());
        if let Some(observer) = &mut self.decoded_frame_observer {
            observer(descriptor);
        }
    }

    fn channel_layout_name(layout: ChannelLayout, channels: u16) -> String {
        if layout == ChannelLayout::MONO {
            "mono".to_owned()
        } else if layout == ChannelLayout::STEREO {
            "stereo".to_owned()
        } else if layout == ChannelLayout::_5POINT1 {
            "5.1".to_owned()
        } else if layout == ChannelLayout::_5POINT1_BACK {
            "5.1-back".to_owned()
        } else if layout == ChannelLayout::_7POINT1 {
            "7.1".to_owned()
        } else {
            format!("{}ch:{layout:?}", channels).to_ascii_lowercase()
        }
    }

    pub fn wrap_with_observer(
        sink: Box<dyn AudioSink + Send>,
        carrier_format: AudioFormat,
        codec_id: Id,
        decoded_frame_observer: Option<DecodedFrameObserver>,
    ) -> anyhow::Result<Self> {
        let out_spec = sink.specs();
        let out_sample_fmt = Self::map_output_format_to_ffmpeg_sample(out_spec.format)?;
        let out_layout = Self::ffmpeg_layout(out_spec.layout);
        let out_rate = out_spec.rate;

        let codec_name = match codec_id {
            Id::AC3 => "AC3",
            Id::EAC3 => "E-AC3",
            Id::DTS => "DTS",
            _ => "Unknown",
        };

        let codec = decoder::find(codec_id)
            .ok_or_else(|| anyhow::anyhow!("{} decoder not found in ffmpeg", codec_name))?;
        let decoder = context::Context::new_with_codec(codec).decoder().audio()?;

        Ok(Self {
            decoder,
            codec_id,
            resampler: None,
            frame: frame::Audio::empty(),
            pending: Vec::with_capacity(4096),
            inner: sink,
            out_spec,
            out_sample_fmt,
            out_layout,
            out_rate,
            decoded_frame_observer,
            last_decoded_descriptor: None,
            resampler_input: None,
            carrier_format,
        })
    }

    /// Close FFmpeg state and release the wrapped output sink.
    pub fn finish(mut self) -> anyhow::Result<Box<dyn AudioSink + Send>> {
        drop(self.decoder);
        drop(self.resampler.take());
        Ok(self.inner)
    }

    /// Convert `self.frame` (decoder output) to bytes in the *requested* format.
    fn convert_and_pack_current_frame(&mut self) -> anyhow::Result<Vec<u8>> {
        let swr = self
            .resampler
            .as_mut()
            .expect("resampler must be initialized");
        let mut out_frame = frame::Audio::empty();

        swr.run(&self.frame, &mut out_frame)
            .context("swresample run")?;

        let packed = Self::frame_to_interleaved_bytes(&out_frame, self.out_sample_fmt)?;
        Self::reorder_ffmpeg_packed_output(packed, self.out_spec)
    }

    fn ffmpeg_packed_positions(layout: AudioLayout) -> &'static [AudioChannel] {
        // A native AVChannelLayout bitmask has a canonical packed order. Its
        // 7.1 order puts rear channels before side channels, while Open
        // Cinema's stable working bus deliberately puts side channels first.
        const FFMPEG_SURROUND_71: [AudioChannel; 8] = [
            AudioChannel::FrontLeft,
            AudioChannel::FrontRight,
            AudioChannel::FrontCenter,
            AudioChannel::Lfe,
            AudioChannel::RearLeft,
            AudioChannel::RearRight,
            AudioChannel::SideLeft,
            AudioChannel::SideRight,
        ];
        match layout {
            AudioLayout::Surround71 => &FFMPEG_SURROUND_71,
            _ => layout.positions(),
        }
    }

    fn reorder_ffmpeg_packed_output(bytes: Vec<u8>, spec: AudioSpec) -> anyhow::Result<Vec<u8>> {
        let source_positions = Self::ffmpeg_packed_positions(spec.layout);
        let output_positions = spec.layout.positions();
        if source_positions == output_positions {
            return Ok(bytes);
        }

        anyhow::ensure!(
            bytes.len() % spec.frame_bytes() == 0,
            "FFmpeg packed output is not frame aligned"
        );
        let mapping = output_positions
            .iter()
            .map(|position| {
                source_positions
                    .iter()
                    .position(|candidate| candidate == position)
                    .with_context(|| format!("FFmpeg output omitted channel {position:?}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let sample_bytes = spec.format.bytes_per_sample();
        let mut reordered = Vec::with_capacity(bytes.len());
        for frame in bytes.chunks_exact(spec.frame_bytes()) {
            for source_index in &mapping {
                let start = source_index * sample_bytes;
                reordered.extend_from_slice(&frame[start..start + sample_bytes]);
            }
        }
        Ok(reordered)
    }

    fn frame_to_interleaved_bytes(frame: &frame::Audio, fmt: Sample) -> anyhow::Result<Vec<u8>> {
        let ch = frame.channels() as usize;
        let n = frame.samples();

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
            other => anyhow::bail!(
                "Unsupported packed sample format in frame_to_interleaved_bytes: {:?}",
                other
            ),
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

    fn iec61937_payload_to_be(
        payload: &[u8],
        fmt: AudioFormat,
        syncword: &[u8],
    ) -> Option<Vec<u8>> {
        // Rebuild the bitstream as a sequence of 16-bit words in big-endian byte order
        let mut be_words: Vec<u8> = match fmt {
            // Most common: 16-bit IEC words captured as S16_LE
            AudioFormat::S16Le => {
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
            AudioFormat::S32Le => {
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
            AudioFormat::F32Le => {
                eprintln!(
                    "iec61937_payload_to_be: unsupported capture format {:?}",
                    fmt
                );
                return None;
            }
        };

        // Find syncword in the reconstructed bitstream
        let sync_len = syncword.len();
        let mut sync_idx = None;
        for i in 0..be_words.len().saturating_sub(sync_len - 1) {
            if &be_words[i..i + sync_len] == syncword {
                sync_idx = Some(i);
                break;
            }
        }
        let sync_idx = sync_idx?;

        Some(be_words.split_off(sync_idx))
    }

    fn iec61937_payload_to_ac3_be(payload: &[u8], fmt: AudioFormat) -> Option<Vec<u8>> {
        // AC-3 syncword: 0x0B77 (16-bit, big-endian)
        Self::iec61937_payload_to_be(payload, fmt, &[0x0B, 0x77])
    }

    fn iec61937_payload_to_eac3_be(payload: &[u8], fmt: AudioFormat) -> Option<Vec<u8>> {
        // E-AC3 syncword: 0x0B77 (same as AC-3)
        Self::iec61937_payload_to_be(payload, fmt, &[0x0B, 0x77])
    }

    fn iec61937_payload_to_dts_be(payload: &[u8], fmt: AudioFormat) -> Option<Vec<u8>> {
        // DTS syncword: 0x7FFE8001 (32-bit, big-endian)
        Self::iec61937_payload_to_be(payload, fmt, &[0x7F, 0xFE, 0x80, 0x01])
    }
}

impl AudioSink for Ac3DecoderSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        // 1. Accumulate raw AC-3 bytes
        self.pending.extend_from_slice(bytes);

        // 2. While we have at least one whole frame, decode it
        while let Some((preamble, frame_bytes)) = Self::take_ac3_burst(&mut self.pending) {
            let raw_data = match preamble.stream_type {
                StreamType::Ac3 => {
                    Self::iec61937_payload_to_ac3_be(&frame_bytes, self.carrier_format)
                }
                StreamType::Dts1 | StreamType::Dts2 | StreamType::Dts3 => {
                    Self::iec61937_payload_to_dts_be(&frame_bytes, self.carrier_format)
                }
                StreamType::EAc3 => {
                    Self::iec61937_payload_to_eac3_be(&frame_bytes, self.carrier_format)
                }
                _ => None,
            };

            if let Some(raw_ac3) = raw_data {
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
                    let codec_name = match self.codec_id {
                        Id::AC3 => "AC3",
                        Id::EAC3 => "E-AC3",
                        Id::DTS => "DTS",
                        _ => "CODEC",
                    };
                    eprintln!(
                        "{}: send_packet failed for this burst: {e}, dropping it",
                        codec_name
                    );
                    // Just skip this burst and keep going; decoder will sync later if data becomes valid
                    continue;
                }

                // Pull all available frames
                loop {
                    match self.decoder.receive_frame(&mut self.frame) {
                        Ok(_) => {
                            self.observe_decoded_frame();
                            self.ensure_swr_initialized()?;
                            let buf = self.convert_and_pack_current_frame()?;
                            self.inner.write(&buf)?;
                        }
                        Err(ffmpeg_next::Error::Other { errno }) if errno == EAGAIN => break, // needs more packets
                        Err(ffmpeg_next::Error::Eof) => break, // flushed
                        Err(e) => return Err(anyhow::anyhow!("receive_frame failed: {e}")),
                    }
                }
            } else {
                let codec_name = match preamble.stream_type {
                    StreamType::Ac3 => "AC-3",
                    StreamType::EAc3 => "E-AC3",
                    StreamType::Dts1 | StreamType::Dts2 | StreamType::Dts3 => "DTS",
                    _ => "codec",
                };
                eprintln!("No {} syncword found in burst, dropping", codec_name);
            }
        }

        Ok(())
    }

    fn specs(&self) -> AudioSpec {
        self.out_spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iec61937_detector::StreamType::Ac3;

    #[test]
    fn ffmpeg_seven_one_output_is_reordered_to_the_open_cinema_bus() {
        let spec = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        // FFmpeg native-mask order: FL FR FC LFE RL RR SL SR.
        let packed = (1_u32..=8)
            .map(f32::from_bits)
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let reordered = Ac3DecoderSink::reorder_ffmpeg_packed_output(packed, spec).unwrap();
        let samples = reordered
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()).to_bits())
            .collect::<Vec<_>>();

        assert_eq!(samples, [1, 2, 3, 4, 7, 8, 5, 6]);
    }

    struct TestSink(AudioSpec);

    impl AudioSink for TestSink {
        fn write(&mut self, _bytes: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }

        fn specs(&self) -> AudioSpec {
            self.0
        }
    }

    #[test]
    fn carrier_format_is_independent_from_adaptive_output_format() {
        ffmpeg_next::init().unwrap();
        let output = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        let decoder = Ac3DecoderSink::wrap_with_observer(
            Box::new(TestSink(output)),
            AudioFormat::S16Le,
            Id::AC3,
            None,
        )
        .unwrap();

        assert_eq!(decoder.carrier_format, AudioFormat::S16Le);
        assert_eq!(decoder.specs().format, AudioFormat::F32Le);
        assert_eq!(
            Ac3DecoderSink::iec61937_payload_to_ac3_be(
                &[0x77, 0x0B, 0x34, 0x12],
                decoder.carrier_format,
            ),
            Some(vec![0x0B, 0x77, 0x12, 0x34])
        );
    }

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
        let burst = Ac3DecoderSink::take_ac3_burst(bytes.as_mut());
        assert!(
            burst.is_none(),
            "an incomplete IEC burst must wait for more bytes"
        );
        assert_eq!(
            bytes.len(),
            size_before,
            "the partial burst must be preserved"
        );

        Ok(())
    }
}
