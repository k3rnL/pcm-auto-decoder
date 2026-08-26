use crate::iec61937_detector::StreamType;
use crate::status_protocol::{DetectionConfidence, DetectionMode, TransportFraming};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Ac3,
    EAc3,
    Dts,
    Unsupported(u8),
}

impl Codec {
    pub fn from_stream_type(stream_type: StreamType) -> Self {
        match stream_type {
            StreamType::Ac3 => Self::Ac3,
            StreamType::EAc3 => Self::EAc3,
            StreamType::Dts1 | StreamType::Dts2 | StreamType::Dts3 => Self::Dts,
            StreamType::Unknown(value) => Self::Unsupported(value),
        }
    }

    pub fn status_name(self) -> String {
        match self {
            Self::Ac3 => "ac3".to_owned(),
            Self::EAc3 => "eac3".to_owned(),
            Self::Dts => "dts".to_owned(),
            Self::Unsupported(value) => format!("iec61937-0x{value:02x}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StableMode {
    Unknown,
    Pcm,
    Encoded(Codec),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetectionTransition {
    pub previous: StableMode,
    pub current: StableMode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DetectionUpdate {
    pub stable_mode: StableMode,
    pub reported_mode: DetectionMode,
    pub framing: TransportFraming,
    pub codec: Option<Codec>,
    pub confidence: DetectionConfidence,
    pub transition: Option<DetectionTransition>,
    pub buffer_current_chunk: bool,
    pub flush_candidate_buffer: bool,
    pub rejected_candidate: Option<Codec>,
}

pub struct DetectionTracker {
    pcm_confirmation_frames: usize,
    encoded_confirmation_bursts: usize,
    stable_mode: StableMode,
    pcm_observation_frames: usize,
    candidate_codec: Option<Codec>,
    candidate_hits: usize,
    candidate_quiet_frames: usize,
}

impl DetectionTracker {
    pub fn new(pcm_confirmation_frames: usize, encoded_confirmation_bursts: usize) -> Self {
        Self {
            pcm_confirmation_frames: pcm_confirmation_frames.max(1),
            encoded_confirmation_bursts: encoded_confirmation_bursts.max(1),
            stable_mode: StableMode::Unknown,
            pcm_observation_frames: 0,
            candidate_codec: None,
            candidate_hits: 0,
            candidate_quiet_frames: 0,
        }
    }

    #[cfg(test)]
    pub fn observe(&mut self, observed_codec: Option<Codec>) -> DetectionUpdate {
        self.observe_frames(observed_codec, 1)
    }

    pub fn observe_frames(
        &mut self,
        observed_codec: Option<Codec>,
        observed_frames: usize,
    ) -> DetectionUpdate {
        let observed_frames = observed_frames.max(1);
        let previous = self.stable_mode;
        let had_candidate = self.candidate_codec.is_some();
        let mut flush_candidate_buffer = false;
        let mut rejected_candidate = None;

        match (self.stable_mode, observed_codec) {
            (StableMode::Unknown, Some(codec)) => {
                self.observe_candidate(codec);
                self.pcm_observation_frames = 0;
                if self.candidate_hits >= self.encoded_confirmation_bursts {
                    self.stable_mode = StableMode::Encoded(codec);
                    self.clear_candidate();
                    flush_candidate_buffer = true;
                }
            }
            (StableMode::Unknown, None) => {
                self.pcm_observation_frames =
                    self.pcm_observation_frames.saturating_add(observed_frames);
                if let Some(rejected) = self.age_candidate(observed_frames) {
                    flush_candidate_buffer = true;
                    rejected_candidate = Some(rejected);
                }
                if self.candidate_codec.is_none()
                    && self.pcm_observation_frames >= self.pcm_confirmation_frames
                {
                    self.stable_mode = StableMode::Pcm;
                    flush_candidate_buffer = true;
                }
            }
            (StableMode::Pcm, Some(codec)) => {
                self.observe_candidate(codec);
                if self.candidate_hits >= self.encoded_confirmation_bursts {
                    self.stable_mode = StableMode::Encoded(codec);
                    self.clear_candidate();
                    flush_candidate_buffer = true;
                }
            }
            (StableMode::Pcm, None) => {
                if let Some(rejected) = self.age_candidate(observed_frames) {
                    flush_candidate_buffer = true;
                    rejected_candidate = Some(rejected);
                }
            }
            (StableMode::Encoded(active), Some(codec)) if codec == active => {
                if had_candidate {
                    flush_candidate_buffer = true;
                    rejected_candidate = self.candidate_codec;
                }
                self.clear_candidate();
                self.pcm_observation_frames = 0;
            }
            (StableMode::Encoded(_), Some(codec)) => {
                self.observe_candidate(codec);
                self.pcm_observation_frames = 0;
                if self.candidate_hits >= self.encoded_confirmation_bursts {
                    self.stable_mode = StableMode::Encoded(codec);
                    self.clear_candidate();
                    flush_candidate_buffer = true;
                }
            }
            (StableMode::Encoded(_), None) => {
                if self.candidate_codec.is_some() {
                    if let Some(rejected) = self.age_candidate(observed_frames) {
                        flush_candidate_buffer = true;
                        rejected_candidate = Some(rejected);
                    }
                } else {
                    self.pcm_observation_frames =
                        self.pcm_observation_frames.saturating_add(observed_frames);
                    if self.pcm_observation_frames >= self.pcm_confirmation_frames {
                        self.stable_mode = StableMode::Pcm;
                        self.pcm_observation_frames = 0;
                    }
                }
            }
        }

        let transition = (previous != self.stable_mode).then_some(DetectionTransition {
            previous,
            current: self.stable_mode,
        });
        // Ordinary PCM-looking input is silenced while startup detection is
        // unresolved, not retained for later playback. Retaining the complete
        // PCM confirmation window would put audio permanently behind video.
        // Only bytes belonging to a possible encoded burst need buffering.
        let buffer_current_chunk = had_candidate
            || self.candidate_codec.is_some()
            || matches!(observed_codec, Some(codec) if previous != StableMode::Encoded(codec));

        let (reported_mode, framing, codec, confidence) =
            if let Some(candidate) = self.candidate_codec {
                (
                    DetectionMode::Detecting,
                    TransportFraming::Iec61937,
                    Some(candidate),
                    self.candidate_confidence(),
                )
            } else {
                match self.stable_mode {
                    StableMode::Unknown => (
                        DetectionMode::Detecting,
                        TransportFraming::Unknown,
                        None,
                        DetectionConfidence {
                            score: (self.pcm_observation_frames as f64
                                / self.pcm_confirmation_frames as f64)
                                .min(1.0),
                            observations: self.pcm_observation_frames as u32,
                            required_observations: self.pcm_confirmation_frames as u32,
                        },
                    ),
                    StableMode::Pcm => (
                        DetectionMode::Pcm,
                        TransportFraming::Pcm,
                        None,
                        DetectionConfidence {
                            score: 1.0,
                            observations: self.pcm_confirmation_frames as u32,
                            required_observations: self.pcm_confirmation_frames as u32,
                        },
                    ),
                    StableMode::Encoded(codec) => (
                        DetectionMode::Decoding,
                        TransportFraming::Iec61937,
                        Some(codec),
                        DetectionConfidence {
                            score: 1.0,
                            observations: self.encoded_confirmation_bursts as u32,
                            required_observations: self.encoded_confirmation_bursts as u32,
                        },
                    ),
                }
            };

        DetectionUpdate {
            stable_mode: self.stable_mode,
            reported_mode,
            framing,
            codec,
            confidence,
            transition,
            buffer_current_chunk,
            flush_candidate_buffer,
            rejected_candidate,
        }
    }

    fn observe_candidate(&mut self, codec: Codec) {
        if self.candidate_codec == Some(codec) {
            self.candidate_hits = self.candidate_hits.saturating_add(1);
        } else {
            self.candidate_codec = Some(codec);
            self.candidate_hits = 1;
        }
        self.candidate_quiet_frames = 0;
    }

    fn age_candidate(&mut self, observed_frames: usize) -> Option<Codec> {
        let candidate = self.candidate_codec?;
        self.candidate_quiet_frames = self.candidate_quiet_frames.saturating_add(observed_frames);
        if self.candidate_quiet_frames >= self.pcm_confirmation_frames {
            self.clear_candidate();
            Some(candidate)
        } else {
            None
        }
    }

    fn clear_candidate(&mut self) {
        self.candidate_codec = None;
        self.candidate_hits = 0;
        self.candidate_quiet_frames = 0;
    }

    fn candidate_confidence(&self) -> DetectionConfidence {
        DetectionConfidence {
            score: self.candidate_hits as f64 / self.encoded_confirmation_bursts as f64,
            observations: self.candidate_hits as u32,
            required_observations: self.encoded_confirmation_bursts as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_is_detecting_until_pcm_window_is_complete() {
        let mut tracker = DetectionTracker::new(3, 2);
        let first = tracker.observe(None);
        assert_eq!(first.reported_mode, DetectionMode::Detecting);
        assert_eq!(first.confidence.score, 1.0 / 3.0);
        assert!(!first.buffer_current_chunk);
        assert_eq!(
            tracker.observe(None).reported_mode,
            DetectionMode::Detecting
        );
        let stable = tracker.observe(None);
        assert_eq!(stable.stable_mode, StableMode::Pcm);
        assert_eq!(stable.reported_mode, DetectionMode::Pcm);
        assert!(stable.flush_candidate_buffer);
        assert!(!stable.buffer_current_chunk);
    }

    #[test]
    fn pcm_window_is_measured_in_frames_not_pipewire_callback_blocks() {
        let mut small_blocks = DetectionTracker::new(1_200, 2);
        for _ in 0..4 {
            assert_eq!(
                small_blocks.observe_frames(None, 240).stable_mode,
                StableMode::Unknown
            );
        }
        assert_eq!(
            small_blocks.observe_frames(None, 240).stable_mode,
            StableMode::Pcm
        );

        let mut mixed_blocks = DetectionTracker::new(1_200, 2);
        assert_eq!(
            mixed_blocks.observe_frames(None, 1_024).stable_mode,
            StableMode::Unknown
        );
        assert_eq!(
            mixed_blocks.observe_frames(None, 176).stable_mode,
            StableMode::Pcm
        );
    }

    #[test]
    fn one_false_preamble_does_not_switch_pcm_to_decoding() {
        let mut tracker = DetectionTracker::new(3, 2);
        for _ in 0..3 {
            tracker.observe(None);
        }
        let candidate = tracker.observe(Some(Codec::Ac3));
        assert_eq!(candidate.stable_mode, StableMode::Pcm);
        assert_eq!(candidate.reported_mode, DetectionMode::Detecting);
        assert!(candidate.buffer_current_chunk);

        tracker.observe(None);
        tracker.observe(None);
        let abandoned = tracker.observe(None);
        assert_eq!(abandoned.stable_mode, StableMode::Pcm);
        assert_eq!(abandoned.reported_mode, DetectionMode::Pcm);
        assert!(abandoned.flush_candidate_buffer);
        assert_eq!(abandoned.rejected_candidate, Some(Codec::Ac3));
    }

    #[test]
    fn confirmed_bursts_switch_and_missing_framing_falls_back_with_hysteresis() {
        let mut tracker = DetectionTracker::new(3, 2);
        tracker.observe(None);
        let first = tracker.observe(Some(Codec::EAc3));
        assert_eq!(first.reported_mode, DetectionMode::Detecting);
        let confirmed = tracker.observe(Some(Codec::EAc3));
        assert_eq!(confirmed.stable_mode, StableMode::Encoded(Codec::EAc3));
        assert!(confirmed.transition.is_some());
        assert!(confirmed.flush_candidate_buffer);

        assert_eq!(
            tracker.observe(None).stable_mode,
            StableMode::Encoded(Codec::EAc3)
        );
        assert_eq!(
            tracker.observe(None).stable_mode,
            StableMode::Encoded(Codec::EAc3)
        );
        let pcm = tracker.observe(None);
        assert_eq!(pcm.stable_mode, StableMode::Pcm);
        assert_eq!(pcm.reported_mode, DetectionMode::Pcm);
    }

    #[test]
    fn codec_change_requires_confirmation() {
        let mut tracker = DetectionTracker::new(4, 2);
        tracker.observe(Some(Codec::Ac3));
        tracker.observe(Some(Codec::Ac3));
        let candidate = tracker.observe(Some(Codec::Dts));
        assert_eq!(candidate.stable_mode, StableMode::Encoded(Codec::Ac3));
        assert_eq!(candidate.reported_mode, DetectionMode::Detecting);
        let changed = tracker.observe(Some(Codec::Dts));
        assert_eq!(changed.stable_mode, StableMode::Encoded(Codec::Dts));
    }

    #[test]
    fn pcm_ac3_menu_and_dts_follow_one_deterministic_mode_sequence() {
        let mut tracker = DetectionTracker::new(2, 2);
        let observations = [
            None,
            None,
            Some(Codec::Ac3),
            Some(Codec::Ac3),
            None,
            None,
            Some(Codec::Dts),
            Some(Codec::Dts),
        ];
        let transitions = observations
            .into_iter()
            .filter_map(|codec| tracker.observe(codec).transition)
            .collect::<Vec<_>>();

        assert_eq!(
            transitions,
            vec![
                DetectionTransition {
                    previous: StableMode::Unknown,
                    current: StableMode::Pcm,
                },
                DetectionTransition {
                    previous: StableMode::Pcm,
                    current: StableMode::Encoded(Codec::Ac3),
                },
                DetectionTransition {
                    previous: StableMode::Encoded(Codec::Ac3),
                    current: StableMode::Pcm,
                },
                DetectionTransition {
                    previous: StableMode::Pcm,
                    current: StableMode::Encoded(Codec::Dts),
                },
            ]
        );
    }
}
