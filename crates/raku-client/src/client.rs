use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select, unbounded};
use parking_lot::Mutex;
use polling::{Event, Events, Poller};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use uuid::Uuid;

use raku_protocol::MAX_WIRE_MESSAGE_BYTES;
use raku_protocol::{
    ClientMessage, Command, PROTOCOL_VERSION, ReplayCursor, Request, ResponseOutcome,
    ResponsePayload, RpcError, SequencedEvent, ServerMessage,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BUFFERED_EVENTS_PER_RUNTIME: usize = 4096;

enum Outgoing {
    Message(ClientMessage),
    Shutdown,
}

struct ClientInner {
    outgoing: Sender<Outgoing>,
    pending: Mutex<HashMap<Uuid, Sender<Result<ResponsePayload, RpcError>>>>,
    sessions: Mutex<HashMap<(Uuid, Uuid), Sender<SequencedEvent>>>,
    pending_events: Mutex<HashMap<(Uuid, Uuid), VecDeque<SequencedEvent>>>,
    task_state_subscribers: Mutex<Vec<Sender<u64>>>,
    last_sequences: Mutex<HashMap<(Uuid, Uuid), LastSequence>>,
    disconnected: AtomicBool,
}

#[derive(Clone, Copy)]
struct LastSequence {
    epoch: Uuid,
    sequence: u64,
}

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

impl DaemonClient {
    pub fn connect(address: &str, token: String) -> anyhow::Result<Self> {
        Self::connect_with_resume(address, token, Vec::new())
    }

    pub fn connect_with_resume(
        address: &str,
        token: String,
        resume_from: Vec<ReplayCursor>,
    ) -> anyhow::Result<Self> {
        let last_sequences = resume_from
            .iter()
            .map(|cursor| {
                (
                    (cursor.session_id, cursor.runtime_id),
                    LastSequence {
                        epoch: cursor.epoch,
                        sequence: cursor.sequence,
                    },
                )
            })
            .collect();
        let url = daemon_url(address)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WIRE_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WIRE_MESSAGE_BYTES));
        let (mut socket, _) =
            tungstenite::client::connect_with_config(url.as_str(), Some(config), 3)
                .context("could not connect to Raku daemon")?;
        set_client_read_timeout(&mut socket, Some(Duration::from_secs(5)))?;
        write_json(
            &mut socket,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token,
                resume_from,
            },
        )?;
        let hello = read_server_message(&mut socket)?;
        match hello {
            ServerMessage::Hello {
                protocol_version, ..
            } if protocol_version == PROTOCOL_VERSION => {}
            ServerMessage::Hello {
                protocol_version, ..
            } => bail!(
                "daemon protocol {protocol_version} does not match desktop protocol {PROTOCOL_VERSION}"
            ),
            ServerMessage::Rejected { message } => bail!("daemon rejected connection: {message}"),
            other => bail!("daemon sent an invalid handshake response: {other:?}"),
        }
        // Steady-state reads wait on a `ReadableSignal`, then never block.
        set_client_read_timeout(&mut socket, None)?;
        let readable = ReadableSignal::new(client_tcp_stream(&socket)?)
            .context("could not watch the Raku daemon connection")?;

        let (outgoing, outgoing_rx) = unbounded();
        let inner = Arc::new(ClientInner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            pending_events: Mutex::new(HashMap::new()),
            task_state_subscribers: Mutex::new(Vec::new()),
            last_sequences: Mutex::new(last_sequences),
            disconnected: AtomicBool::new(false),
        });
        let thread_inner = inner.clone();
        std::thread::Builder::new()
            .name("raku-daemon-client".into())
            .spawn(move || run_client(socket, readable, outgoing_rx, thread_inner))
            .context("could not start Raku daemon client thread")?;
        Ok(Self { inner })
    }

    pub fn subscribe(&self, session_id: Uuid, runtime_id: Uuid) -> Receiver<SequencedEvent> {
        let (events, receiver) = unbounded();
        let key = (session_id, runtime_id);
        let mut sessions = self.inner.sessions.lock();
        sessions.insert(key, events.clone());
        // Keep the subscription lock while draining the pre-subscription
        // replay queue. The socket thread takes these locks in the same order,
        // so a new live event cannot overtake older replayed events here.
        if let Some(buffered) = self.inner.pending_events.lock().remove(&key) {
            for event in buffered {
                let _ = events.send(event);
            }
        }
        receiver
    }

    /// Whether two handles send through the same WebSocket connection.
    ///
    /// The daemon supervisor publishes replacement clients after a managed
    /// restart. Runtime adapters use this identity check to ignore the
    /// subscription's initial snapshot and wait for an actual replacement.
    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn is_disconnected(&self) -> bool {
        self.inner.disconnected.load(Ordering::Acquire)
    }

    pub fn unsubscribe(&self, session_id: Uuid, runtime_id: Uuid) {
        self.inner.sessions.lock().remove(&(session_id, runtime_id));
    }

    pub fn subscribe_task_state(&self) -> Receiver<u64> {
        let (events, receiver) = unbounded();
        self.inner.task_state_subscribers.lock().push(events);
        receiver
    }

    pub fn request(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<ResponsePayload> {
        self.begin_request(session_id, runtime_id, command)?.wait()
    }

    /// Queues a request now and returns a handle that waits for its reply.
    ///
    /// The request is ordered against every other message this client sends
    /// at the moment of this call, not when the reply is awaited. A caller on
    /// the UI thread can therefore keep its sends in order while a background
    /// thread does the waiting.
    pub fn begin_request(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<PendingResponse> {
        // Copying a large local snapshot can outlive an ordinary RPC. Keep
        // the UI asynchronous without reporting a timeout while the daemon
        // is still publishing a valid backup or restore handoff.
        let timeout = if matches!(
            &command,
            Command::CreateBackup { .. }
                | Command::CreateAutomaticBackup { .. }
                | Command::StageRestore { .. }
        ) {
            Duration::from_secs(30 * 60)
        } else {
            REQUEST_TIMEOUT
        };
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Raku daemon is disconnected");
        }
        let request_id = Uuid::new_v4();
        let (response, response_rx) = bounded(1);
        self.inner.pending.lock().insert(request_id, response);
        // The socket thread marks the connection closed and then fails every
        // pending request. One registered after that sweep would otherwise
        // wait out the full timeout for a reply that can never arrive.
        if self.inner.disconnected.load(Ordering::Acquire) {
            self.inner.pending.lock().remove(&request_id);
            bail!("Raku daemon is disconnected");
        }
        let message = ClientMessage::Request(Request {
            request_id,
            session_id,
            runtime_id,
            command,
        });
        if self
            .inner
            .outgoing
            .send(Outgoing::Message(message))
            .is_err()
        {
            self.inner.pending.lock().remove(&request_id);
            bail!("Raku daemon connection is closed");
        }
        Ok(PendingResponse {
            inner: self.inner.clone(),
            request_id,
            response: response_rx,
            deadline: Instant::now() + timeout,
        })
    }

    pub fn notify(
        &self,
        session_id: Uuid,
        runtime_id: Uuid,
        command: Command,
    ) -> anyhow::Result<()> {
        if self.inner.disconnected.load(Ordering::Acquire) {
            bail!("Raku daemon is disconnected");
        }
        self.inner
            .outgoing
            .send(Outgoing::Message(ClientMessage::Request(Request {
                // The nil request id is reserved for fire-and-forget controls;
                // the daemon executes them in the runtime mailbox but does
                // not allocate or send a response.
                request_id: Uuid::nil(),
                session_id,
                runtime_id,
                command,
            })))
            .map_err(|_| anyhow!("Raku daemon connection is closed"))
    }

    pub fn last_sequences(&self) -> Vec<ReplayCursor> {
        self.inner
            .last_sequences
            .lock()
            .iter()
            .map(|(&(session_id, runtime_id), cursor)| ReplayCursor {
                session_id,
                runtime_id,
                epoch: cursor.epoch,
                sequence: cursor.sequence,
            })
            .collect()
    }

    pub fn shutdown(&self) {
        let _ = self.inner.outgoing.send(Outgoing::Shutdown);
    }
}

/// A request already sent to the daemon, awaiting its reply.
pub struct PendingResponse {
    inner: Arc<ClientInner>,
    request_id: Uuid,
    response: Receiver<Result<ResponsePayload, RpcError>>,
    /// Fixed when the request was sent, so waiting on a queue of replies in
    /// order cannot stretch each one's timeout past its own.
    deadline: Instant,
}

impl PendingResponse {
    /// Blocks until the reply, a disconnect, or the request's deadline.
    pub fn wait(self) -> anyhow::Result<ResponsePayload> {
        match self.response.recv_deadline(self.deadline) {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(error)) => Err(anyhow!(error.message)),
            Err(error) => {
                self.inner.pending.lock().remove(&self.request_id);
                Err(anyhow!("timed out waiting for Raku daemon: {error}"))
            }
        }
    }
}

fn daemon_url(address: &str) -> anyhow::Result<String> {
    let normalized = if address.starts_with("ws://") || address.starts_with("wss://") {
        address.to_owned()
    } else {
        format!("ws://{address}")
    };
    let mut url = url::Url::parse(&normalized).context("Raku daemon address is invalid")?;
    url.set_path("/v1");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

fn run_client(
    mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
    readable: ReadableSignal,
    outgoing: Receiver<Outgoing>,
    inner: Arc<ClientInner>,
) {
    // The hello read may have buffered replayed events behind it, which no
    // readiness event will report; drain once before the first wait.
    let mut drain = true;
    'connection: loop {
        if drain {
            drain = false;
            loop {
                match read_nonblocking(&mut socket) {
                    Ok(Message::Text(text)) => {
                        let Ok(message) = serde_json::from_str::<ServerMessage>(text.as_ref())
                        else {
                            continue;
                        };
                        if !deliver_server_message(message, &inner) {
                            break 'connection;
                        }
                    }
                    Ok(Message::Close(_)) => break 'connection,
                    Ok(Message::Ping(_)) => {
                        let _ = socket.flush();
                    }
                    Ok(_) => {}
                    // Drained. Level-triggered interest reports anything an
                    // interrupted read left behind as soon as it is rearmed.
                    Err(tungstenite::Error::Io(error)) if retryable_io(&error) => break,
                    Err(_) => break 'connection,
                }
            }
            if readable.rearm().is_err() {
                break;
            }
        }
        select! {
            recv(outgoing) -> message => {
                let mut message = message.ok();
                while let Some(next) = message {
                    match next {
                        Outgoing::Message(message) => {
                            if write_json(&mut socket, &message).is_err() {
                                break 'connection;
                            }
                        }
                        Outgoing::Shutdown => {
                            let _ = write_json(&mut socket, &ClientMessage::Shutdown);
                            let _ = socket.flush();
                            break 'connection;
                        }
                    }
                    message = outgoing.try_recv().ok();
                }
            }
            recv(readable.ready) -> ready => {
                if ready.is_err() {
                    break;
                }
                drain = true;
            }
        }
    }

    inner.disconnected.store(true, Ordering::Release);
    let pending = std::mem::take(&mut *inner.pending.lock());
    for (_, response) in pending {
        let _ = response.send(Err(RpcError {
            message: "Raku daemon disconnected".into(),
        }));
    }
    // Closing the desktop transport is not evidence that a daemon-owned
    // provider exited. Drop the subscription senders so runtime adapters can
    // hand off to a replacement client and ask the daemon whether the same
    // runtime still exists. Real provider exits arrive through the replayable
    // `processExited` event emitted by the daemon.
    drop(std::mem::take(&mut *inner.sessions.lock()));
    inner.task_state_subscribers.lock().clear();
}

/// Routes one server message. Returns `false` once the daemon says it is
/// shutting down, which ends the connection.
fn deliver_server_message(message: ServerMessage, inner: &ClientInner) -> bool {
    match message {
        ServerMessage::Response {
            request_id,
            outcome,
        } => {
            if let Some(pending) = inner.pending.lock().remove(&request_id) {
                let result = match outcome {
                    ResponseOutcome::Ok { payload } => Ok(payload),
                    ResponseOutcome::Error { error } => Err(error),
                };
                let _ = pending.send(result);
            }
        }
        ServerMessage::Event(event) => {
            let should_deliver = {
                let mut sequences = inner.last_sequences.lock();
                let previous = sequences
                    .entry((event.session_id, event.runtime_id))
                    .or_insert(LastSequence {
                        epoch: event.epoch,
                        sequence: 0,
                    });
                if previous.epoch == event.epoch && event.sequence <= previous.sequence {
                    false
                } else {
                    previous.epoch = event.epoch;
                    previous.sequence = event.sequence;
                    true
                }
            };
            if should_deliver {
                let key = (event.session_id, event.runtime_id);
                let sessions = inner.sessions.lock();
                if let Some(events) = sessions.get(&key) {
                    let _ = events.send(event);
                } else {
                    let mut pending = inner.pending_events.lock();
                    let buffered = pending.entry(key).or_default();
                    buffered.push_back(event);
                    while buffered.len() > MAX_BUFFERED_EVENTS_PER_RUNTIME {
                        buffered.pop_front();
                    }
                }
            }
        }
        ServerMessage::TaskStateChanged { revision } => {
            inner
                .task_state_subscribers
                .lock()
                .retain(|subscriber| subscriber.send(revision).is_ok());
        }
        ServerMessage::ShuttingDown => return false,
        ServerMessage::Hello { .. } | ServerMessage::Rejected { .. } => {}
    }
    true
}

/// Reports on a channel when the daemon socket becomes readable.
///
/// The socket thread used to read with a 25 ms timeout so it could also
/// drain outgoing requests between reads. That added up to 25 ms before each
/// request reached the daemon and woke the thread 40 times a second while
/// idle. A helper thread now blocks in the OS poller and signals readiness, so
/// the socket thread can block on the socket and its request queue at once
/// with `select!`. Interest is one-shot: drain to `WouldBlock`, then
/// [`Self::rearm`].
struct ReadableSignal {
    poller: Arc<Poller>,
    /// A duplicate handle of the watched socket, owned so the registration
    /// can be removed before the descriptor closes.
    stream: TcpStream,
    ready: Receiver<()>,
    stopped: Arc<AtomicBool>,
}

impl ReadableSignal {
    fn new(stream: &TcpStream) -> io::Result<Self> {
        let stream = stream.try_clone()?;
        let poller = Arc::new(Poller::new()?);
        // SAFETY: `Drop` deregisters `stream` before it is closed.
        unsafe { poller.add(&stream, Event::readable(0))? };
        let (ready_tx, ready) = bounded(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_poller = poller.clone();
        let thread_stopped = stopped.clone();
        let spawned = std::thread::Builder::new()
            .name("raku-daemon-client-ready".into())
            .spawn(move || {
                let mut events = Events::new();
                loop {
                    events.clear();
                    let woke = thread_poller.wait(&mut events, None);
                    if thread_stopped.load(Ordering::Acquire) {
                        return;
                    }
                    match woke {
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => return,
                    }
                    if !events.is_empty()
                        && let Err(TrySendError::Disconnected(())) = ready_tx.try_send(())
                    {
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            let _ = poller.delete(&stream);
            return Err(error);
        }
        Ok(Self {
            poller,
            stream,
            ready,
            stopped,
        })
    }

    fn rearm(&self) -> io::Result<()> {
        self.poller.modify(&self.stream, Event::readable(0))
    }
}

impl Drop for ReadableSignal {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.poller.notify();
        let _ = self.poller.delete(&self.stream);
    }
}

fn client_tcp_stream(socket: &WebSocket<MaybeTlsStream<TcpStream>>) -> io::Result<&TcpStream> {
    match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => Ok(stream),
        MaybeTlsStream::Rustls(stream) => Ok(&stream.sock),
        #[allow(unreachable_patterns)]
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported Raku daemon transport",
        )),
    }
}

fn set_client_nonblocking(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    nonblocking: bool,
) -> io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_nonblocking(nonblocking),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_nonblocking(nonblocking),
        #[allow(unreachable_patterns)]
        _ => Ok(()),
    }
}

/// One read that returns `WouldBlock` instead of waiting. The socket stays
/// blocking otherwise, so request writes complete in order.
fn read_nonblocking(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> tungstenite::Result<Message> {
    set_client_nonblocking(socket, true)?;
    let message = socket.read();
    set_client_nonblocking(socket, false)?;
    message
}

fn set_client_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Option<Duration>,
) -> io::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(timeout),
        #[allow(unreachable_patterns)]
        _ => Ok(()),
    }
}

fn retryable_io(error: &io::Error) -> bool {
    retryable_error(error)
}

fn retryable_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        ) {
            return true;
        }
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::EAGAIN)
            || error.raw_os_error() == Some(libc::EWOULDBLOCK)
        {
            return true;
        }
    }
    error.source().is_some_and(retryable_error)
}

fn write_json<S: io::Read + io::Write, T: serde::Serialize>(
    socket: &mut WebSocket<S>,
    value: &T,
) -> anyhow::Result<()> {
    let payload = serde_json::to_string(value)?;
    socket.send(Message::Text(payload.into()))?;
    Ok(())
}

fn read_server_message(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> anyhow::Result<ServerMessage> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(text.as_ref())?),
            Message::Ping(_) => socket.flush()?,
            Message::Close(_) => bail!("Raku daemon closed during handshake"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A save the daemon never acknowledges must fail as soon as the socket
    /// closes, so the caller re-marks and retries instead of waiting out the
    /// request timeout or assuming success.
    #[test]
    fn pending_requests_fail_promptly_when_the_connection_drops() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let _hello = socket.read().unwrap();
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        daemon_version: "test".into(),
                    })
                    .unwrap()
                    .into(),
                ))
                .unwrap();
            // Receive the request, then drop the connection unanswered.
            let _request = socket.read().unwrap();
        });
        let client = DaemonClient::connect(&address, "token".into()).unwrap();
        let pending = client
            .begin_request(Uuid::nil(), Uuid::nil(), Command::LoadComposerDrafts)
            .unwrap();
        let started = std::time::Instant::now();
        assert!(pending.wait().is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            client
                .begin_request(Uuid::nil(), Uuid::nil(), Command::LoadComposerDrafts)
                .is_err()
        );
        server.join().unwrap();
    }

    #[test]
    fn daemon_endpoint_accepts_addresses_and_secure_urls() {
        assert_eq!(
            daemon_url("127.0.0.1:4312").unwrap(),
            "ws://127.0.0.1:4312/v1"
        );
        assert_eq!(
            daemon_url("wss://raku.example.test/old?ignored=1").unwrap(),
            "wss://raku.example.test/v1"
        );
    }
}
