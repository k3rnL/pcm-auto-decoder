use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const PROTOCOL_VERSION: u16 = 2;
const MAX_REQUEST_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Starting,
    Ready,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionMode {
    Unknown,
    Detecting,
    Pcm,
    Decoding,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportFraming {
    Unknown,
    Pcm,
    Iec61937,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDescriptor {
    pub sample_rate: u32,
    pub sample_format: String,
    pub channels: u16,
    pub channel_layout: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportDescriptor {
    pub framing: TransportFraming,
    pub sample_rate: u32,
    pub sample_format: String,
    pub channels: u16,
    pub channel_layout: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamIdentity {
    pub capture_node_name: String,
    pub capture_stream_name: String,
    pub output_node_name: String,
    pub output_stream_name: String,
    pub node_group_name: String,
}

impl StreamIdentity {
    pub fn for_instance(instance_id: &str) -> Self {
        let base = format!("open-cinema.decoder.{instance_id}");
        Self {
            capture_node_name: format!("{base}.capture"),
            capture_stream_name: format!("{base}.capture.stream"),
            output_node_name: format!("{base}.output"),
            output_stream_name: format!("{base}.output.stream"),
            node_group_name: base,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionConfidence {
    pub score: f64,
    pub observations: u32,
    pub required_observations: u32,
}

impl DetectionConfidence {
    pub fn unknown(required_observations: u32) -> Self {
        Self {
            score: 0.0,
            observations: 0,
            required_observations,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusError {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecoderStatus {
    pub protocol_version: u16,
    pub message_type: String,
    pub instance_id: String,
    pub sequence: u64,
    pub timestamp: String,
    pub lifecycle: Lifecycle,
    pub mode: DetectionMode,
    pub transport: TransportDescriptor,
    pub codec: Option<String>,
    pub decoded: Option<AudioDescriptor>,
    pub emitted: AudioDescriptor,
    pub confidence: DetectionConfidence,
    pub streams: StreamIdentity,
    pub errors: Vec<StatusError>,
}

impl DecoderStatus {
    pub fn starting(
        instance_id: String,
        transport: TransportDescriptor,
        emitted: AudioDescriptor,
        required_observations: u32,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            message_type: "status".to_owned(),
            streams: StreamIdentity::for_instance(&instance_id),
            instance_id,
            sequence: 0,
            timestamp: timestamp(),
            lifecycle: Lifecycle::Starting,
            mode: DetectionMode::Unknown,
            transport,
            codec: None,
            decoded: None,
            emitted,
            confidence: DetectionConfidence::unknown(required_observations),
            errors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusRequest {
    protocol_version: u16,
    message_type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse<'a> {
    protocol_version: u16,
    message_type: &'static str,
    instance_id: &'a str,
    sequence: u64,
    timestamp: String,
    error: StatusError,
}

#[derive(Clone)]
struct Client {
    id: u64,
    writer: Arc<Mutex<UnixStream>>,
}

#[derive(Clone)]
pub struct StatusReporter {
    state: Arc<Mutex<DecoderStatus>>,
    clients: Arc<Mutex<Vec<Client>>>,
}

impl StatusReporter {
    pub fn new(initial: DecoderStatus) -> Self {
        Self {
            state: Arc::new(Mutex::new(initial)),
            clients: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn snapshot(&self) -> DecoderStatus {
        self.state.lock().expect("status mutex poisoned").clone()
    }

    pub fn update(&self, update: impl FnOnce(&mut DecoderStatus)) -> DecoderStatus {
        let snapshot = {
            let mut state = self.state.lock().expect("status mutex poisoned");
            update(&mut state);
            state.sequence = state.sequence.saturating_add(1);
            state.timestamp = timestamp();
            state.clone()
        };
        self.broadcast(&snapshot);
        snapshot
    }

    fn add_client(&self, id: u64, writer: Arc<Mutex<UnixStream>>) {
        self.clients
            .lock()
            .expect("client mutex poisoned")
            .push(Client { id, writer });
    }

    fn remove_client(&self, id: u64) {
        self.clients
            .lock()
            .expect("client mutex poisoned")
            .retain(|client| client.id != id);
    }

    fn broadcast(&self, status: &DecoderStatus) {
        let line = status_line(status);
        self.clients
            .lock()
            .expect("client mutex poisoned")
            .retain(|client| write_line(&client.writer, &line).is_ok());
    }

    #[cfg(test)]
    fn client_count(&self) -> usize {
        self.clients.lock().expect("client mutex poisoned").len()
    }
}

pub struct StatusServer {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl StatusServer {
    pub fn start(path: impl AsRef<Path>, reporter: StatusReporter) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.file_type().is_socket() => {
                anyhow::bail!("refusing to replace non-socket path {}", path.display());
            }
            Ok(_) if UnixStream::connect(&path).is_ok() => {
                anyhow::bail!(
                    "decoder status socket is already active at {}",
                    path.display()
                );
            }
            Ok(_) => fs::remove_file(&path)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let accept_thread = thread::Builder::new()
            .name("decoder-status-listener".to_owned())
            .spawn(move || accept_loop(listener, reporter, thread_stop))?;

        Ok(Self {
            path,
            stop,
            accept_thread: Some(accept_thread),
        })
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for StatusServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequenceObservation {
    First,
    Next,
    Gap { expected: u64, received: u64 },
    Stale,
}

#[cfg(test)]
#[derive(Default)]
pub struct SequenceTracker {
    instance_id: Option<String>,
    sequence: Option<u64>,
}

#[cfg(test)]
impl SequenceTracker {
    pub fn observe(&mut self, instance_id: &str, sequence: u64) -> SequenceObservation {
        if self.instance_id.as_deref() != Some(instance_id) {
            self.instance_id = Some(instance_id.to_owned());
            self.sequence = Some(sequence);
            return SequenceObservation::First;
        }

        match self.sequence {
            None => {
                self.sequence = Some(sequence);
                SequenceObservation::First
            }
            Some(previous) if sequence == previous.saturating_add(1) => {
                self.sequence = Some(sequence);
                SequenceObservation::Next
            }
            Some(previous) if sequence > previous => {
                self.sequence = Some(sequence);
                SequenceObservation::Gap {
                    expected: previous.saturating_add(1),
                    received: sequence,
                }
            }
            Some(_) => SequenceObservation::Stale,
        }
    }
}

fn accept_loop(listener: UnixListener, reporter: StatusReporter, stop: Arc<AtomicBool>) {
    let next_client_id = AtomicU64::new(1);
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let id = next_client_id.fetch_add(1, Ordering::Relaxed);
                let client_reporter = reporter.clone();
                let client_stop = Arc::clone(&stop);
                let _ = thread::Builder::new()
                    .name(format!("decoder-status-client-{id}"))
                    .spawn(move || handle_client(id, stream, client_reporter, client_stop));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

fn handle_client(id: u64, stream: UnixStream, reporter: StatusReporter, stop: Arc<AtomicBool>) {
    let writer_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let _ = writer_stream.set_nonblocking(true);
    let writer = Arc::new(Mutex::new(writer_stream));
    reporter.add_client(id, Arc::clone(&writer));

    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut reader = BufReader::new(stream);
    let mut request = Vec::new();
    while !stop.load(Ordering::Acquire) {
        request.clear();
        match reader.read_until(b'\n', &mut request) {
            Ok(0) => break,
            Ok(_) if request.len() > MAX_REQUEST_BYTES => {
                send_error(
                    &writer,
                    &reporter.snapshot(),
                    "request_too_large",
                    "request exceeds 8192 bytes",
                    false,
                );
                break;
            }
            Ok(_) => handle_request(&request, &writer, &reporter),
            // `try_clone` duplicates the descriptor but both handles share the
            // socket's O_NONBLOCK flag. The writer is deliberately non-blocking
            // so a client that stops reading cannot stall decoder state
            // publication. Consequently an idle reader returns WouldBlock
            // immediately instead of honoring the read timeout. Yield here or
            // every connected observer consumes an entire CPU core while no
            // request is pending.
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    reporter.remove_client(id);
}

fn handle_request(request: &[u8], writer: &Arc<Mutex<UnixStream>>, reporter: &StatusReporter) {
    let status = reporter.snapshot();
    let request: StatusRequest = match serde_json::from_slice(request) {
        Ok(request) => request,
        Err(_) => {
            send_error(
                writer,
                &status,
                "malformed_request",
                "request is not valid protocol JSON",
                true,
            );
            return;
        }
    };
    if request.protocol_version != PROTOCOL_VERSION {
        send_error(
            writer,
            &status,
            "unsupported_protocol",
            &format!(
                "protocol version {} is unsupported; expected {}",
                request.protocol_version, PROTOCOL_VERSION
            ),
            false,
        );
    } else if request.message_type != "getStatus" {
        send_error(
            writer,
            &status,
            "unsupported_request",
            "messageType must be getStatus",
            true,
        );
    } else {
        let _ = write_line(writer, &status_line(&status));
    }
}

fn send_error(
    writer: &Arc<Mutex<UnixStream>>,
    status: &DecoderStatus,
    code: &str,
    message: &str,
    recoverable: bool,
) {
    let response = ErrorResponse {
        protocol_version: PROTOCOL_VERSION,
        message_type: "error",
        instance_id: &status.instance_id,
        sequence: status.sequence,
        timestamp: timestamp(),
        error: StatusError {
            code: code.to_owned(),
            message: message.to_owned(),
            recoverable,
        },
    };
    if let Ok(line) = serde_json::to_vec(&response) {
        let _ = write_line(writer, &[line, vec![b'\n']].concat());
    }
}

fn status_line(status: &DecoderStatus) -> Vec<u8> {
    let mut line = serde_json::to_vec(status).expect("DecoderStatus must serialize");
    line.push(b'\n');
    line
}

fn write_line(writer: &Arc<Mutex<UnixStream>>, line: &[u8]) -> std::io::Result<()> {
    writer
        .lock()
        .expect("writer mutex poisoned")
        .write_all(line)
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};

    fn initial_status() -> DecoderStatus {
        DecoderStatus::starting(
            "living-room".to_owned(),
            TransportDescriptor {
                framing: TransportFraming::Unknown,
                sample_rate: 48_000,
                sample_format: "s16le".to_owned(),
                channels: 2,
                channel_layout: "stereo".to_owned(),
            },
            AudioDescriptor {
                sample_rate: 48_000,
                sample_format: "float32le".to_owned(),
                channels: 8,
                channel_layout: "7.1".to_owned(),
            },
            2,
        )
    }

    fn connect(path: &Path) -> UnixStream {
        for _ in 0..50 {
            if let Ok(stream) = UnixStream::connect(path) {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                return stream;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("status socket did not become available")
    }

    fn read_json_line(stream: &UnixStream) -> Value {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn status_round_trip_has_complete_versioned_shape() {
        let status = initial_status();
        let value = serde_json::to_value(&status).unwrap();
        let restored: DecoderStatus = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(restored, status);
        assert_eq!(value["protocolVersion"], 2);
        assert_eq!(value["mode"], "unknown");
        assert_eq!(value["transport"]["framing"], "unknown");
        assert_eq!(
            value["streams"]["outputNodeName"],
            "open-cinema.decoder.living-room.output"
        );
        assert_eq!(value["emitted"]["channelLayout"], "7.1");
    }

    #[test]
    fn new_and_reconnected_clients_can_request_complete_latest_status() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decoder.sock");
        let reporter = StatusReporter::new(initial_status());
        let mut server = StatusServer::start(&path, reporter.clone()).unwrap();

        let mut first = connect(&path);
        first
            .write_all(b"{\"protocolVersion\":2,\"messageType\":\"getStatus\"}\n")
            .unwrap();
        assert_eq!(read_json_line(&first)["sequence"], 0);
        drop(first);

        reporter.update(|status| {
            status.lifecycle = Lifecycle::Ready;
            status.mode = DetectionMode::Pcm;
            status.transport.framing = TransportFraming::Pcm;
            status.confidence.score = 1.0;
            status.confidence.observations = 64;
        });

        let mut second = connect(&path);
        second
            .write_all(b"{\"protocolVersion\":2,\"messageType\":\"getStatus\"}\n")
            .unwrap();
        let response = read_json_line(&second);
        assert_eq!(response["sequence"], 1);
        assert_eq!(response["lifecycle"], "ready");
        assert_eq!(response["mode"], "pcm");
        server.shutdown();
    }

    #[test]
    fn incompatible_and_malformed_requests_receive_structured_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decoder.sock");
        let reporter = StatusReporter::new(initial_status());
        let _server = StatusServer::start(&path, reporter).unwrap();
        let mut client = connect(&path);

        client.write_all(b"not-json\n").unwrap();
        let malformed = read_json_line(&client);
        assert_eq!(malformed["messageType"], "error");
        assert_eq!(malformed["error"]["code"], "malformed_request");

        client
            .write_all(b"{\"protocolVersion\":99,\"messageType\":\"getStatus\"}\n")
            .unwrap();
        let incompatible = read_json_line(&client);
        assert_eq!(incompatible["error"]["code"], "unsupported_protocol");
        assert_eq!(incompatible["error"]["recoverable"], false);
    }

    #[test]
    fn events_are_newline_delimited_and_monotonic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decoder.sock");
        let reporter = StatusReporter::new(initial_status());
        let _server = StatusServer::start(&path, reporter.clone()).unwrap();
        let client = connect(&path);
        for _ in 0..50 {
            if reporter.client_count() == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        reporter.update(|status| status.mode = DetectionMode::Detecting);
        reporter.update(|status| status.mode = DetectionMode::Decoding);

        let mut reader = BufReader::new(client);
        let mut first = String::new();
        let mut second = String::new();
        reader.read_line(&mut first).unwrap();
        reader.read_line(&mut second).unwrap();
        assert!(first.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&first).unwrap()["sequence"],
            1
        );
        assert_eq!(
            serde_json::from_str::<Value>(&second).unwrap()["sequence"],
            2
        );
    }

    #[test]
    fn slow_client_is_disconnected_without_blocking_state_publication() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decoder.sock");
        let reporter = StatusReporter::new(initial_status());
        let _server = StatusServer::start(&path, reporter.clone()).unwrap();
        let _client_that_never_reads = connect(&path);
        for _ in 0..50 {
            if reporter.client_count() == 1 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        let large_message = "slow-client-test".repeat(256);
        for _ in 0..10_000 {
            reporter.update(|status| {
                status.errors = vec![StatusError {
                    code: "diagnostic".to_owned(),
                    message: large_message.clone(),
                    recoverable: true,
                }];
            });
            if reporter.client_count() == 0 {
                break;
            }
        }

        assert_eq!(reporter.client_count(), 0);
        assert!(reporter.snapshot().sequence > 0);
    }

    #[test]
    fn sequence_tracker_reports_gap_stale_and_new_instance() {
        let mut tracker = SequenceTracker::default();
        assert_eq!(tracker.observe("a", 4), SequenceObservation::First);
        assert_eq!(tracker.observe("a", 5), SequenceObservation::Next);
        assert_eq!(
            tracker.observe("a", 8),
            SequenceObservation::Gap {
                expected: 6,
                received: 8
            }
        );
        assert_eq!(tracker.observe("a", 7), SequenceObservation::Stale);
        assert_eq!(tracker.observe("b", 1), SequenceObservation::First);
    }

    #[test]
    fn shutdown_cleans_up_socket_and_disconnects_clients() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decoder.sock");
        let reporter = StatusReporter::new(initial_status());
        let mut server = StatusServer::start(&path, reporter).unwrap();
        let mut client = connect(&path);
        assert!(path.exists());
        server.shutdown();
        assert!(!path.exists());

        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut byte = [0_u8];
        match client.read(&mut byte) {
            Ok(0) => {}
            Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
            outcome => panic!("client remained connected after shutdown: {outcome:?}"),
        }
    }

    #[test]
    fn startup_refuses_to_replace_a_non_socket_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decoder.sock");
        fs::write(&path, b"not a socket").unwrap();
        let reporter = StatusReporter::new(initial_status());

        let error = StatusServer::start(&path, reporter).err().unwrap();

        assert!(error.to_string().contains("refusing to replace non-socket"));
        assert_eq!(fs::read(&path).unwrap(), b"not a socket");
    }
}
