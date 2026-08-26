use crate::sinks::{AudioChannel, AudioFormat, AudioSink, AudioSpec};
use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;
use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Keep enough blocks to absorb short non-real-time decoder jitter without
// retaining hundreds of milliseconds after a detection transition.
const QUEUE_DEPTH: usize = 4;
const MAX_QUANTUM_FRAMES: usize = 8192;

#[derive(Default)]
pub struct PipeWireDiagnostics {
    capture_overflows: AtomicU64,
    playback_underruns: AtomicU64,
    playback_overflows: AtomicU64,
}

impl PipeWireDiagnostics {
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.capture_overflows.load(Ordering::Relaxed),
            self.playback_underruns.load(Ordering::Relaxed),
            self.playback_overflows.load(Ordering::Relaxed),
        )
    }
}

struct CaptureCallback {
    free_rx: Receiver<Vec<u8>>,
    free_tx: Sender<Vec<u8>>,
    ready_tx: Sender<Vec<u8>>,
    diagnostics: Arc<PipeWireDiagnostics>,
    maximum_bytes: usize,
}

struct PlaybackCallback {
    free_tx: Sender<Vec<u8>>,
    ready_rx: Receiver<Vec<u8>>,
    diagnostics: Arc<PipeWireDiagnostics>,
    current: Option<Vec<u8>>,
    offset: usize,
    frame_bytes: usize,
    position: Option<NonNull<spa_sys::spa_io_position>>,
}

pub struct PipeWireRuntime {
    _capture_listener: pw::stream::StreamListener<CaptureCallback>,
    _playback_listener: pw::stream::StreamListener<PlaybackCallback>,
    capture_stream: pw::stream::StreamRc,
    playback_stream: pw::stream::StreamRc,
    thread_loop: pw::thread_loop::ThreadLoopRc,
    diagnostics: Arc<PipeWireDiagnostics>,
}

impl PipeWireRuntime {
    // Keeping every externally visible stream identity explicit here prevents
    // capture/output names from being silently coupled or derived differently.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        instance_id: &str,
        capture_stream_name: &str,
        capture_node_name: &str,
        output_stream_name: &str,
        output_node_name: &str,
        node_group_name: &str,
        capture_spec: AudioSpec,
        output_spec: AudioSpec,
        quantum_frames: usize,
    ) -> anyhow::Result<(Self, PipeWireInput, PipeWireSink)> {
        capture_spec.validate()?;
        output_spec.validate()?;
        anyhow::ensure!(quantum_frames > 0, "PipeWire quantum must be positive");
        pw::init();

        let thread_loop = unsafe {
            pw::thread_loop::ThreadLoopRc::new(
                Some(&format!("open-cinema-decoder-{instance_id}")),
                None,
            )
        }
        .context("create PipeWire thread loop")?;
        let context =
            pw::context::ContextRc::new(&thread_loop, None).context("create PipeWire context")?;
        let core = context.connect_rc(None).context("connect to PipeWire")?;
        let diagnostics = Arc::new(PipeWireDiagnostics::default());
        let stream_error = Arc::new(Mutex::new(None));

        let capture_stream = pw::stream::StreamRc::new(
            core.clone(),
            capture_stream_name,
            stream_properties(
                instance_id,
                capture_node_name,
                capture_stream_name,
                node_group_name,
                "Capture",
                "capture",
                capture_spec.channels(),
                quantum_frames,
                capture_spec.rate,
            ),
        )
        .context("create native PipeWire capture stream")?;
        let playback_stream = pw::stream::StreamRc::new(
            core,
            output_stream_name,
            stream_properties(
                instance_id,
                output_node_name,
                output_stream_name,
                node_group_name,
                "Playback",
                "output",
                output_spec.channels(),
                quantum_frames,
                output_spec.rate,
            ),
        )
        .context("create native PipeWire adaptive output stream")?;

        let capture_maximum = MAX_QUANTUM_FRAMES * capture_spec.frame_bytes();
        let output_maximum = MAX_QUANTUM_FRAMES * output_spec.frame_bytes();
        let (capture_free_tx, capture_free_rx) = bounded(QUEUE_DEPTH);
        let (capture_ready_tx, capture_ready_rx) = bounded(QUEUE_DEPTH);
        let (playback_free_tx, playback_free_rx) = bounded(QUEUE_DEPTH);
        let (playback_ready_tx, playback_ready_rx) = bounded(QUEUE_DEPTH);
        for _ in 0..QUEUE_DEPTH {
            capture_free_tx
                .send(Vec::with_capacity(capture_maximum))
                .expect("new capture queue is connected");
            playback_free_tx
                .send(Vec::with_capacity(output_maximum))
                .expect("new playback queue is connected");
        }

        let capture_error = Arc::clone(&stream_error);
        let capture_listener = capture_stream
            .add_local_listener_with_user_data(CaptureCallback {
                free_rx: capture_free_rx,
                free_tx: capture_free_tx.clone(),
                ready_tx: capture_ready_tx,
                diagnostics: Arc::clone(&diagnostics),
                maximum_bytes: capture_maximum,
            })
            .state_changed(move |_, _, _, state| {
                record_stream_error(&capture_error, "capture", state)
            })
            .process(|stream, callback| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    callback
                        .diagnostics
                        .capture_overflows
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                let Some(bytes) = data.data() else {
                    return;
                };
                let Some(end) = offset.checked_add(size) else {
                    return;
                };
                if end > bytes.len() || size > callback.maximum_bytes {
                    callback
                        .diagnostics
                        .capture_overflows
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let Ok(mut queued) = callback.free_rx.try_recv() else {
                    callback
                        .diagnostics
                        .capture_overflows
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                };
                queued.clear();
                queued.extend_from_slice(&bytes[offset..end]);
                if let Err(TrySendError::Full(queued) | TrySendError::Disconnected(queued)) =
                    callback.ready_tx.try_send(queued)
                {
                    let _ = callback.free_tx.try_send(queued);
                    callback
                        .diagnostics
                        .capture_overflows
                        .fetch_add(1, Ordering::Relaxed);
                }
            })
            .register()
            .context("register PipeWire capture callbacks")?;

        let playback_error = Arc::clone(&stream_error);
        let playback_listener = playback_stream
            .add_local_listener_with_user_data(PlaybackCallback {
                free_tx: playback_free_tx.clone(),
                ready_rx: playback_ready_rx.clone(),
                diagnostics: Arc::clone(&diagnostics),
                current: None,
                offset: 0,
                frame_bytes: output_spec.frame_bytes(),
                position: None,
            })
            .state_changed(move |_, _, _, state| {
                record_stream_error(&playback_error, "output", state)
            })
            .io_changed(|_, callback, id, area, size| {
                if id == spa_sys::SPA_IO_Position
                    && size as usize >= size_of::<spa_sys::spa_io_position>()
                {
                    callback.position = NonNull::new(area.cast());
                } else if id == spa_sys::SPA_IO_Position {
                    callback.position = None;
                }
            })
            .process(|stream, callback| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let Some(bytes) = data.data() else {
                    return;
                };
                let requested_frames = callback.position.and_then(|position| {
                    // PipeWire owns this IO area for the registered stream and
                    // keeps it valid until a matching io_changed event replaces
                    // or removes it. Both callbacks run on the same thread loop.
                    let duration = unsafe { position.as_ref().clock.duration };
                    (duration > 0).then_some(duration)
                });
                let writable =
                    playback_writable_bytes(bytes.len(), callback.frame_bytes, requested_frames);
                bytes[..writable].fill(0);
                let mut written = 0;
                while written < writable {
                    if callback.current.is_none() {
                        match callback.ready_rx.try_recv() {
                            Ok(next) => {
                                callback.current = Some(next);
                                callback.offset = 0;
                            }
                            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                        }
                    }
                    let current = callback.current.as_ref().expect("current output exists");
                    let available = current.len().saturating_sub(callback.offset);
                    let copying = available.min(writable - written);
                    bytes[written..written + copying]
                        .copy_from_slice(&current[callback.offset..callback.offset + copying]);
                    written += copying;
                    callback.offset += copying;
                    if callback.offset == current.len() {
                        let mut consumed = callback.current.take().expect("current output exists");
                        consumed.clear();
                        let _ = callback.free_tx.try_send(consumed);
                        callback.offset = 0;
                    }
                }
                if written < writable {
                    callback
                        .diagnostics
                        .playback_underruns
                        .fetch_add(1, Ordering::Relaxed);
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = callback.frame_bytes as i32;
                *chunk.size_mut() = writable as u32;
            })
            .register()
            .context("register PipeWire playback callbacks")?;

        let capture_format = format_parameter(capture_spec)?;
        let output_format = format_parameter(output_spec)?;
        let mut capture_params =
            [Pod::from_bytes(&capture_format).context("build PipeWire capture format pod")?];
        let mut output_params =
            [Pod::from_bytes(&output_format).context("build PipeWire output format pod")?];
        let flags = pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
        capture_stream
            .connect(
                spa::utils::Direction::Input,
                None,
                flags,
                &mut capture_params,
            )
            .context("connect native PipeWire capture stream")?;
        playback_stream
            .connect(
                spa::utils::Direction::Output,
                None,
                flags,
                &mut output_params,
            )
            .context("connect native PipeWire adaptive output stream")?;
        thread_loop.start();

        let input = PipeWireInput {
            ready_rx: capture_ready_rx,
            free_tx: capture_free_tx,
            current: Vec::new(),
            stream_error: Arc::clone(&stream_error),
        };
        let output = PipeWireSink {
            spec: output_spec,
            ready_tx: playback_ready_tx,
            ready_rx: playback_ready_rx,
            free_rx: playback_free_rx,
            free_tx: playback_free_tx,
            stream_error,
            diagnostics: Arc::clone(&diagnostics),
        };
        Ok((
            Self {
                _capture_listener: capture_listener,
                _playback_listener: playback_listener,
                capture_stream,
                playback_stream,
                thread_loop,
                diagnostics: Arc::clone(&diagnostics),
            },
            input,
            output,
        ))
    }

    pub fn diagnostics(&self) -> (u64, u64, u64) {
        self.diagnostics.snapshot()
    }
}

impl Drop for PipeWireRuntime {
    fn drop(&mut self) {
        {
            let _guard = self.thread_loop.lock();
            let _ = self.capture_stream.disconnect();
            let _ = self.playback_stream.disconnect();
        }
        self.thread_loop.stop();
    }
}

pub struct PipeWireInput {
    ready_rx: Receiver<Vec<u8>>,
    free_tx: Sender<Vec<u8>>,
    current: Vec<u8>,
    stream_error: Arc<Mutex<Option<String>>>,
}

impl PipeWireInput {
    pub fn read_chunk(&mut self, stop: &AtomicBool) -> anyhow::Result<Option<&[u8]>> {
        if !self.current.is_empty() {
            let mut previous = std::mem::take(&mut self.current);
            previous.clear();
            let _ = self.free_tx.try_send(previous);
        }
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            check_stream_error(&self.stream_error)?;
            match self.ready_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(bytes) => {
                    self.current = bytes;
                    return Ok(Some(&self.current));
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("PipeWire capture queue disconnected")
                }
            }
        }
    }
}

pub struct PipeWireSink {
    spec: AudioSpec,
    ready_tx: Sender<Vec<u8>>,
    ready_rx: Receiver<Vec<u8>>,
    free_rx: Receiver<Vec<u8>>,
    free_tx: Sender<Vec<u8>>,
    stream_error: Arc<Mutex<Option<String>>>,
    diagnostics: Arc<PipeWireDiagnostics>,
}

impl AudioSink for PipeWireSink {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        check_stream_error(&self.stream_error)?;
        anyhow::ensure!(
            bytes.len() % self.spec.frame_bytes() == 0,
            "adaptive output is not frame aligned"
        );
        let mut queued = match self.free_rx.try_recv() {
            Ok(queued) => queued,
            Err(TryRecvError::Empty) => {
                self.diagnostics
                    .playback_overflows
                    .fetch_add(1, Ordering::Relaxed);
                match self.ready_rx.try_recv() {
                    // Discard the oldest pending block so a temporarily
                    // disconnected downstream graph cannot accumulate latency.
                    Ok(queued) => queued,
                    // The PipeWire callback owns every buffer at this instant.
                    // Dropping this input block preserves capture continuity and
                    // lets the next callback return a reusable buffer.
                    Err(TryRecvError::Empty) => return Ok(()),
                    Err(TryRecvError::Disconnected) => {
                        anyhow::bail!("PipeWire adaptive output queue is unavailable")
                    }
                }
            }
            Err(TryRecvError::Disconnected) => {
                anyhow::bail!("PipeWire adaptive output queue is unavailable")
            }
        };
        queued.clear();
        queued.extend_from_slice(bytes);
        match self.ready_tx.try_send(queued) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(mut queued)) => {
                queued.clear();
                let _ = self.free_tx.try_send(queued);
                self.diagnostics
                    .playback_overflows
                    .fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(TrySendError::Disconnected(mut queued)) => {
                queued.clear();
                let _ = self.free_tx.try_send(queued);
                anyhow::bail!("PipeWire adaptive output queue is unavailable")
            }
        }
    }

    fn specs(&self) -> AudioSpec {
        self.spec
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        while let Ok(mut queued) = self.ready_rx.try_recv() {
            queued.clear();
            let _ = self.free_tx.try_send(queued);
        }
        Ok(())
    }
}

// These fields form the native PipeWire/WirePlumber matching contract and are
// intentionally visible at each call site.
#[allow(clippy::too_many_arguments)]
fn stream_properties(
    instance_id: &str,
    node_name: &str,
    stream_name: &str,
    node_group_name: &str,
    category: &str,
    port: &str,
    channels: u8,
    quantum_frames: usize,
    rate: u32,
) -> pw::properties::PropertiesBox {
    properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => category,
        *pw::keys::MEDIA_ROLE => "Movie",
        *pw::keys::MEDIA_NAME => stream_name,
        *pw::keys::NODE_NAME => node_name,
        *pw::keys::NODE_DESCRIPTION => format!("Open Cinema adaptive decoder {instance_id} {port}"),
        *pw::keys::NODE_GROUP => node_group_name,
        *pw::keys::NODE_LATENCY => format!("{quantum_frames}/{rate}"),
        *pw::keys::NODE_AUTOCONNECT => "false",
        *pw::keys::AUDIO_CHANNELS => channels.to_string(),
        "open-cinema.processor.kind" => "adaptive-decoder",
        "open-cinema.processor.instance" => instance_id,
        "open-cinema.processor.port" => port,
        "open-cinema.stream-role" => port,
        "open-cinema.managed" => "true",
    }
}

fn playback_writable_bytes(
    available_bytes: usize,
    frame_bytes: usize,
    requested_frames: Option<u64>,
) -> usize {
    let available_frames = available_bytes / frame_bytes;
    let requested_frames = requested_frames
        .and_then(|frames| usize::try_from(frames).ok())
        .unwrap_or(available_frames);
    available_frames.min(requested_frames) * frame_bytes
}

fn format_parameter(spec: AudioSpec) -> anyhow::Result<Vec<u8>> {
    spec.validate()?;
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(match spec.format {
        AudioFormat::S16Le => spa::param::audio::AudioFormat::S16LE,
        AudioFormat::S32Le => spa::param::audio::AudioFormat::S32LE,
        AudioFormat::F32Le => spa::param::audio::AudioFormat::F32LE,
    });
    audio_info.set_rate(spec.rate);
    audio_info.set_channels(spec.channels() as u32);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    for (index, channel) in spec.layout.positions().iter().enumerate() {
        positions[index] = pipewire_position(*channel);
    }
    audio_info.set_position(positions);
    let object = spa::pod::Object {
        type_: spa_sys::SPA_TYPE_OBJECT_Format,
        id: spa_sys::SPA_PARAM_EnumFormat,
        properties: audio_info.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .context("serialize PipeWire audio format")?
    .0
    .into_inner();
    Ok(bytes)
}

fn pipewire_position(channel: AudioChannel) -> u32 {
    match channel {
        AudioChannel::Mono => spa_sys::SPA_AUDIO_CHANNEL_MONO,
        AudioChannel::FrontLeft => spa_sys::SPA_AUDIO_CHANNEL_FL,
        AudioChannel::FrontRight => spa_sys::SPA_AUDIO_CHANNEL_FR,
        AudioChannel::FrontCenter => spa_sys::SPA_AUDIO_CHANNEL_FC,
        AudioChannel::Lfe => spa_sys::SPA_AUDIO_CHANNEL_LFE,
        AudioChannel::SideLeft => spa_sys::SPA_AUDIO_CHANNEL_SL,
        AudioChannel::SideRight => spa_sys::SPA_AUDIO_CHANNEL_SR,
        AudioChannel::RearLeft => spa_sys::SPA_AUDIO_CHANNEL_RL,
        AudioChannel::RearRight => spa_sys::SPA_AUDIO_CHANNEL_RR,
    }
}

fn record_stream_error(
    error_state: &Arc<Mutex<Option<String>>>,
    role: &str,
    state: pw::stream::StreamState,
) {
    if let pw::stream::StreamState::Error(message) = state {
        *error_state.lock().expect("stream error lock poisoned") =
            Some(format!("PipeWire {role} stream failed: {message}"));
    }
}

fn check_stream_error(error_state: &Arc<Mutex<Option<String>>>) -> anyhow::Result<()> {
    if let Some(message) = error_state
        .lock()
        .expect("stream error lock poisoned")
        .as_ref()
    {
        anyhow::bail!(message.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sinks::AudioLayout;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn position_tables_are_explicit_for_every_supported_layout() {
        assert_eq!(AudioLayout::Mono.positions(), &[AudioChannel::Mono]);
        assert_eq!(AudioLayout::Stereo.positions(), &STEREO_POSITIONS);
        assert_eq!(
            AudioLayout::Surround71.positions(),
            &[
                AudioChannel::FrontLeft,
                AudioChannel::FrontRight,
                AudioChannel::FrontCenter,
                AudioChannel::Lfe,
                AudioChannel::SideLeft,
                AudioChannel::SideRight,
                AudioChannel::RearLeft,
                AudioChannel::RearRight,
            ]
        );
    }

    fn spec() -> AudioSpec {
        AudioSpec {
            format: AudioFormat::F32Le,
            rate: 48_000,
            layout: AudioLayout::Stereo,
        }
    }

    #[test]
    fn supported_format_negotiation_serializes_an_explicit_pod() {
        let pod = format_parameter(spec()).unwrap();
        assert!(!pod.is_empty());

        let incompatible = AudioSpec { rate: 0, ..spec() };
        assert!(format_parameter(incompatible).is_err());
    }

    #[test]
    fn stream_requests_the_managed_processing_quantum() {
        let properties = stream_properties(
            "decoder-0",
            "capture-node",
            "capture-stream",
            "decoder-group",
            "Capture",
            "capture",
            2,
            512,
            48_000,
        );

        assert_eq!(properties.get("node.latency"), Some("512/48000"));
    }

    #[test]
    fn playback_writes_only_the_current_graph_cycle() {
        assert_eq!(playback_writable_bytes(32_768, 32, Some(512)), 16_384);
        assert_eq!(playback_writable_bytes(32_768, 32, Some(2_048)), 32_768);
        assert_eq!(playback_writable_bytes(32_768, 32, None), 32_768);
    }

    #[test]
    fn stream_errors_and_capture_disconnects_propagate_to_the_worker() {
        let stream_error = Arc::new(Mutex::new(None));
        record_stream_error(
            &stream_error,
            "capture",
            pw::stream::StreamState::Error("negotiation failed".to_owned()),
        );
        assert!(
            check_stream_error(&stream_error)
                .unwrap_err()
                .to_string()
                .contains("capture stream failed")
        );

        let (ready_tx, ready_rx) = bounded::<Vec<u8>>(1);
        drop(ready_tx);
        let (free_tx, _free_rx) = bounded(1);
        let mut input = PipeWireInput {
            ready_rx,
            free_tx,
            current: Vec::new(),
            stream_error: Arc::new(Mutex::new(None)),
        };
        let stopped = AtomicBool::new(false);
        assert!(
            input
                .read_chunk(&stopped)
                .unwrap_err()
                .to_string()
                .contains("capture queue disconnected")
        );
    }

    #[test]
    fn stop_interrupts_capture_wait_without_a_disconnect_error() {
        let (_ready_tx, ready_rx) = bounded::<Vec<u8>>(1);
        let (free_tx, _free_rx) = bounded(1);
        let mut input = PipeWireInput {
            ready_rx,
            free_tx,
            current: Vec::new(),
            stream_error: Arc::new(Mutex::new(None)),
        };
        assert!(input.read_chunk(&AtomicBool::new(true)).unwrap().is_none());
    }

    #[test]
    fn playback_rejects_misalignment_and_drops_when_the_ready_queue_is_full() {
        let diagnostics = Arc::new(PipeWireDiagnostics::default());
        let (ready_tx, ready_rx) = bounded(1);
        ready_tx.send(vec![0; spec().frame_bytes()]).unwrap();
        let (free_tx, free_rx) = bounded(1);
        free_tx.send(Vec::with_capacity(64)).unwrap();
        let mut sink = PipeWireSink {
            spec: spec(),
            ready_tx,
            ready_rx,
            free_rx,
            free_tx,
            stream_error: Arc::new(Mutex::new(None)),
            diagnostics: Arc::clone(&diagnostics),
        };

        assert!(
            sink.write(&[0])
                .unwrap_err()
                .to_string()
                .contains("frame aligned")
        );
        sink.write(&vec![0; spec().frame_bytes()]).unwrap();
        assert_eq!(diagnostics.snapshot().2, 1);
    }

    #[test]
    fn playback_replaces_the_oldest_block_during_downstream_backpressure() {
        let diagnostics = Arc::new(PipeWireDiagnostics::default());
        let (ready_tx, ready_rx) = bounded(1);
        ready_tx.send(vec![1; spec().frame_bytes()]).unwrap();
        let (free_tx, free_rx) = bounded(1);
        let mut sink = PipeWireSink {
            spec: spec(),
            ready_tx,
            ready_rx: ready_rx.clone(),
            free_rx,
            free_tx,
            stream_error: Arc::new(Mutex::new(None)),
            diagnostics: Arc::clone(&diagnostics),
        };

        let newest = vec![2; spec().frame_bytes()];
        sink.write(&newest).unwrap();

        assert_eq!(ready_rx.try_recv().unwrap(), newest);
        assert_eq!(diagnostics.snapshot().2, 1);
    }

    const STEREO_POSITIONS: [AudioChannel; 2] = [AudioChannel::FrontLeft, AudioChannel::FrontRight];
}
