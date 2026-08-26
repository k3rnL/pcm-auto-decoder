use anyhow::Context;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// The suffix is part of the public audio-format vocabulary and distinguishes
// these explicit little-endian wire formats from future/native-endian formats.
#[allow(clippy::enum_variant_names)]
pub enum AudioFormat {
    S16Le,
    S32Le,
    F32Le,
}

impl AudioFormat {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "s16le" => Ok(Self::S16Le),
            "s32le" => Ok(Self::S32Le),
            "f32le" | "float32le" => Ok(Self::F32Le),
            _ => anyhow::bail!("unsupported audio format {value:?}"),
        }
    }

    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::S16Le => 2,
            Self::S32Le | Self::F32Le => 4,
        }
    }

    pub const fn status_name(self) -> &'static str {
        match self {
            Self::S16Le => "s16le",
            Self::S32Le => "s32le",
            Self::F32Le => "float32le",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioChannel {
    Mono,
    FrontLeft,
    FrontRight,
    FrontCenter,
    Lfe,
    SideLeft,
    SideRight,
    RearLeft,
    RearRight,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioLayout {
    Mono,
    Stereo,
    Surround51Side,
    Surround51Rear,
    Surround71,
}

const MONO: [AudioChannel; 1] = [AudioChannel::Mono];
const STEREO: [AudioChannel; 2] = [AudioChannel::FrontLeft, AudioChannel::FrontRight];
const SURROUND_51_SIDE: [AudioChannel; 6] = [
    AudioChannel::FrontLeft,
    AudioChannel::FrontRight,
    AudioChannel::FrontCenter,
    AudioChannel::Lfe,
    AudioChannel::SideLeft,
    AudioChannel::SideRight,
];
const SURROUND_51_REAR: [AudioChannel; 6] = [
    AudioChannel::FrontLeft,
    AudioChannel::FrontRight,
    AudioChannel::FrontCenter,
    AudioChannel::Lfe,
    AudioChannel::RearLeft,
    AudioChannel::RearRight,
];
const SURROUND_71: [AudioChannel; 8] = [
    AudioChannel::FrontLeft,
    AudioChannel::FrontRight,
    AudioChannel::FrontCenter,
    AudioChannel::Lfe,
    AudioChannel::SideLeft,
    AudioChannel::SideRight,
    AudioChannel::RearLeft,
    AudioChannel::RearRight,
];

impl AudioLayout {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "mono" => Ok(Self::Mono),
            "stereo" | "2.0" => Ok(Self::Stereo),
            "5.1" | "5.1-side" => Ok(Self::Surround51Side),
            "5.1-rear" | "5.1-back" => Ok(Self::Surround51Rear),
            "7.1" => Ok(Self::Surround71),
            "6" | "6ch" => {
                anyhow::bail!("six-channel layout is ambiguous; use 5.1-side or 5.1-rear")
            }
            _ => anyhow::bail!("unsupported audio layout {value:?}"),
        }
    }

    pub const fn status_name(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Stereo => "stereo",
            Self::Surround51Side => "5.1-side",
            Self::Surround51Rear => "5.1-rear",
            Self::Surround71 => "7.1",
        }
    }

    pub const fn positions(self) -> &'static [AudioChannel] {
        match self {
            Self::Mono => &MONO,
            Self::Stereo => &STEREO,
            Self::Surround51Side => &SURROUND_51_SIDE,
            Self::Surround51Rear => &SURROUND_51_REAR,
            Self::Surround71 => &SURROUND_71,
        }
    }

    pub const fn channels(self) -> u8 {
        self.positions().len() as u8
    }

    pub fn accepts_position_preserving_input(self, input: Self) -> bool {
        input.positions().iter().all(|channel| {
            self.positions().contains(channel)
                || (*channel == AudioChannel::Mono
                    && self.positions().contains(&AudioChannel::FrontCenter))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioSpec {
    pub format: AudioFormat,
    pub rate: u32,
    pub layout: AudioLayout,
}

impl AudioSpec {
    pub fn validate(self) -> anyhow::Result<Self> {
        anyhow::ensure!(self.rate > 0, "sample rate must be positive");
        Ok(self)
    }

    pub const fn channels(self) -> u8 {
        self.layout.channels()
    }

    pub const fn frame_bytes(self) -> usize {
        self.channels() as usize * self.format.bytes_per_sample()
    }
}

impl fmt::Display for AudioSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} Hz {}",
            self.format.status_name(),
            self.rate,
            self.layout.status_name()
        )
    }
}

pub trait AudioSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
    fn specs(&self) -> AudioSpec;
    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

pub(crate) struct FileSink {
    file: File,
    spec: AudioSpec,
}

impl FileSink {
    pub(crate) fn open(path: &Path, spec: AudioSpec) -> anyhow::Result<Self> {
        let file = File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .context("open output file")?;
        Ok(Self {
            file,
            spec: spec.validate()?,
        })
    }
}

impl AudioSink for FileSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.file.write_all(bytes).context("write output file")
    }

    fn specs(&self) -> AudioSpec {
        self.spec
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.file.flush().context("flush output file")
    }
}

pub struct PcmNormalizer {
    input: AudioSpec,
    output: AudioSpec,
    mapping: Vec<Option<usize>>,
}

impl PcmNormalizer {
    pub fn new(input: AudioSpec, output: AudioSpec) -> anyhow::Result<Self> {
        anyhow::ensure!(
            input.rate == output.rate,
            "PCM rate conversion is not yet supported"
        );
        anyhow::ensure!(
            output
                .layout
                .accepts_position_preserving_input(input.layout),
            "output layout {} would narrow input layout {}",
            output.layout.status_name(),
            input.layout.status_name()
        );
        let mapping = output
            .layout
            .positions()
            .iter()
            .map(|output_channel| {
                input
                    .layout
                    .positions()
                    .iter()
                    .position(|input_channel| input_channel == output_channel)
                    .or_else(|| {
                        (*output_channel == AudioChannel::FrontCenter
                            && input.layout == AudioLayout::Mono)
                            .then_some(0)
                    })
            })
            .collect();
        Ok(Self {
            input: input.validate()?,
            output: output.validate()?,
            mapping,
        })
    }

    pub fn convert(&self, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            bytes.len() % self.input.frame_bytes() == 0,
            "PCM input is not frame aligned"
        );
        let frames = bytes.len() / self.input.frame_bytes();
        let mut output = Vec::with_capacity(frames * self.output.frame_bytes());
        for frame_index in 0..frames {
            for source_index in &self.mapping {
                let sample = source_index
                    .map(|channel| self.decode_sample(bytes, frame_index, channel))
                    .unwrap_or(0.0);
                self.encode_sample(sample, &mut output);
            }
        }
        Ok(output)
    }

    pub fn silence_for_input(&self, bytes: &[u8]) -> Vec<u8> {
        let frames = bytes.len() / self.input.frame_bytes();
        vec![0; frames * self.output.frame_bytes()]
    }

    fn decode_sample(&self, bytes: &[u8], frame: usize, channel: usize) -> f32 {
        let sample_size = self.input.format.bytes_per_sample();
        let start = frame * self.input.frame_bytes() + channel * sample_size;
        match self.input.format {
            AudioFormat::S16Le => {
                i16::from_le_bytes([bytes[start], bytes[start + 1]]) as f32 / 32768.0
            }
            AudioFormat::S32Le => {
                i32::from_le_bytes([
                    bytes[start],
                    bytes[start + 1],
                    bytes[start + 2],
                    bytes[start + 3],
                ]) as f32
                    / 2_147_483_648.0
            }
            AudioFormat::F32Le => f32::from_le_bytes([
                bytes[start],
                bytes[start + 1],
                bytes[start + 2],
                bytes[start + 3],
            ]),
        }
    }

    fn encode_sample(&self, sample: f32, output: &mut Vec<u8>) {
        let sample = sample.clamp(-1.0, 1.0);
        match self.output.format {
            AudioFormat::S16Le => {
                output.extend_from_slice(&((sample * i16::MAX as f32).round() as i16).to_le_bytes())
            }
            AudioFormat::S32Le => {
                output.extend_from_slice(&((sample * i32::MAX as f32).round() as i32).to_le_bytes())
            }
            AudioFormat::F32Le => output.extend_from_slice(&sample.to_le_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ambiguous_six_channel_layout() {
        assert!(AudioLayout::parse("6ch").is_err());
        assert_eq!(AudioLayout::parse("5.1-side").unwrap().channels(), 6);
        assert_eq!(AudioLayout::parse("5.1-rear").unwrap().channels(), 6);
        assert_eq!(AudioLayout::parse("7.1").unwrap().channels(), 8);
    }

    #[test]
    fn expands_stereo_s16_to_position_preserving_f32_7_1() {
        let input = AudioSpec {
            format: AudioFormat::S16Le,
            rate: 48_000,
            layout: AudioLayout::Stereo,
        };
        let output = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        let converter = PcmNormalizer::new(input, output).unwrap();
        let converted = converter.convert(&[0xff, 0x7f, 0x00, 0x80]).unwrap();
        let samples = converted
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect::<Vec<_>>();

        assert_eq!(samples.len(), 8);
        assert!(samples[0] > 0.99);
        assert_eq!(samples[1], -1.0);
        assert_eq!(&samples[2..], &[0.0; 6]);
    }

    #[test]
    fn expands_five_one_side_without_moving_channels_and_silences_rears() {
        let input = AudioSpec {
            format: AudioFormat::S16Le,
            rate: 48_000,
            layout: AudioLayout::Surround51Side,
        };
        let output = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        let input_samples = [1000_i16, 2000, 3000, 4000, 5000, 6000];
        let bytes = input_samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let converted = PcmNormalizer::new(input, output)
            .unwrap()
            .convert(&bytes)
            .unwrap();
        let samples = converted
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect::<Vec<_>>();

        assert_eq!(samples.len(), 8);
        for (actual, expected) in samples[..6].iter().zip(input_samples) {
            assert!((actual - expected as f32 / i16::MAX as f32).abs() < 0.0001);
        }
        assert_eq!(&samples[6..], &[0.0, 0.0]);
    }

    #[test]
    fn seven_one_mapping_preserves_every_position() {
        let input = AudioSpec {
            format: AudioFormat::S16Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        let output = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        let input_samples = [100_i16, 200, 300, 400, 500, 600, 700, 800];
        let bytes = input_samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let converted = PcmNormalizer::new(input, output)
            .unwrap()
            .convert(&bytes)
            .unwrap();
        let samples = converted
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect::<Vec<_>>();

        assert_eq!(samples.len(), 8);
        assert!(samples.iter().all(|sample| *sample != 0.0));
    }

    #[test]
    fn rejects_implicit_narrowing() {
        let wide = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Surround71,
        };
        let stereo = AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Stereo,
        };
        assert!(PcmNormalizer::new(wide, stereo).is_err());
    }
}
