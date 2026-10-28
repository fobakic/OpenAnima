//! Named-pipe server.
//!
//! The server never touches overlay state directly: it parses frames, pushes
//! commands into a queue and waits for the main (UI) thread to answer. That
//! keeps every Win32 window operation on the thread that owns the message
//! loop, while IPC clients stay fully asynchronous.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use windows::core::HSTRING;
use windows::Win32::Foundation::{HANDLE, LocalFree, HLOCAL};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

use crate::proto::{Event, Request, Response, MAX_FRAME, PIPE_NAME};

/// Command handed from an IPC thread to the UI thread.
pub enum Command {
    /// A client request awaiting a reply.
    Request { req: Request, reply: Sender<Response> },
    /// A notification to broadcast to every client.
    #[allow(dead_code)]
    Broadcast(Event),
}

#[derive(Clone)]
pub struct Broadcaster {
    clients: Arc<Mutex<Vec<ClientHandle>>>,
}

impl Broadcaster {
    pub fn broadcast(&self, event: Event) {
        let clients = self.clients.lock().unwrap();
        for client in clients.iter() {
            let _ = client.events.send(event.clone());
        }
    }
}

/// Shared handle to the running server.
pub struct IpcServer {
    /// Queue consumed by the UI thread.
    pub commands: Receiver<Command>,
    /// Sender clone handed to IPC threads.
    #[allow(dead_code)]
    pub command_tx: Sender<Command>,
    /// Connected clients, used for broadcast.
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    /// Stop flag for the accept loop.
    #[allow(dead_code)]
    stop: Arc<Mutex<bool>>,
    /// Wake-up event, stored as an integer so the struct stays `Send`.
    wakeup: usize,
}

struct ClientHandle {
    id: u64,
    events: Sender<Event>,
}

impl IpcServer {
    /// Starts the accept loop on a background thread.
    pub fn start() -> std::io::Result<Arc<IpcServer>> {
        let (command_tx, commands) = channel();
        let clients: Arc<Mutex<Vec<ClientHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Mutex::new(false));
        let wakeup = unsafe { CreateEventW(None, true, false, None)? }.0 as usize;

        let server = Arc::new(IpcServer {
            commands,
            command_tx: command_tx.clone(),
            clients: clients.clone(),
            stop: stop.clone(),
            wakeup,
        });

        {
            let command_tx = command_tx.clone();
            let clients = clients.clone();
            let stop = stop.clone();
            let wakeup = server.wakeup;
            thread::Builder::new()
                .name("anima-ipc-accept".into())
                .spawn(move || accept_loop(&command_tx, &clients, &stop, wakeup))
                .ok();
        }

        Ok(server)
    }

    /// Wakes the UI thread so it drains the command queue immediately.
    /// Called by IPC threads right after enqueuing a command.
    #[allow(dead_code)]
    pub fn signal(&self) {
        unsafe {
            let _ = SetEvent(self.wakeup_handle());
        }
    }

    /// Called by the UI thread once the queue has been drained.
    pub fn reset(&self) {
        unsafe {
            let _ = ResetEvent(self.wakeup_handle());
        }
    }

    /// The wake-up event as a Win32 handle.
    pub fn wakeup_handle(&self) -> HANDLE {
        HANDLE(self.wakeup as *mut std::ffi::c_void)
    }

    /// Broadcasts an event to every connected client.
    pub fn broadcast(&self, event: Event) {
        self.broadcaster().broadcast(event);
    }

    /// Obtains a cloneable, Send+Sync broadcaster handle.
    pub fn broadcaster(&self) -> Broadcaster {
        Broadcaster {
            clients: self.clients.clone(),
        }
    }

    /// Number of currently connected clients.
    #[allow(dead_code)]
    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }
}

/// Builds a SECURITY_ATTRIBUTES descriptor granting Full Access to Current User (OW) and Admins (BA).
/// Prevents unprivileged or other user processes from accessing the pipe.
fn create_pipe_security() -> (Option<SECURITY_ATTRIBUTES>, PSECURITY_DESCRIPTOR) {
    let sddl = windows::core::w!("D:P(A;;GA;;;OW)(A;;GA;;;BA)");
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let res = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    };
    if res.is_ok() && !sd.is_invalid() {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        (Some(sa), sd)
    } else {
        (None, PSECURITY_DESCRIPTOR::default())
    }
}

fn accept_loop(
    command_tx: &Sender<Command>,
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
    stop: &Arc<Mutex<bool>>,
    wakeup: usize,
) {
    let next_id = AtomicU64::new(1);
    let pipe_name = HSTRING::from(PIPE_NAME);
    let (sa, sd) = create_pipe_security();

    loop {
        if *stop.lock().unwrap() {
            break;
        }

        let sa_ptr = sa.as_ref().map(|s| s as *const _);
        let handle = unsafe {
            CreateNamedPipeW(
                &pipe_name,
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                sa_ptr,
            )
        };

        if handle.is_invalid() {
            thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }

        // Block until a client connects to this pipe instance
        let connected = unsafe { ConnectNamedPipe(handle, None) };
        let ok = if connected.is_ok() {
            true
        } else {
            let code = unsafe { windows::Win32::Foundation::GetLastError() };
            code == windows::Win32::Foundation::ERROR_PIPE_CONNECTED
        };

        if !ok {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
            if *stop.lock().unwrap() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }

        let client_id = next_id.fetch_add(1, Ordering::Relaxed);
        let command_tx = command_tx.clone();
        let clients = clients.clone();
        let handle_value = handle.0 as usize;

        thread::Builder::new()
            .name(format!("openanima-ipc-client-{client_id}"))
            .spawn(move || {
                serve_client(handle_value, client_id, command_tx, clients, wakeup);
            })
            .ok();
    }

    if !sd.is_invalid() {
        unsafe {
            let _ = LocalFree(HLOCAL(sd.0));
        }
    }
}

fn serve_client(
    handle_value: usize,
    client_id: u64,
    command_tx: Sender<Command>,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    wakeup: usize,
) {
    let handle = HANDLE(handle_value as *mut std::ffi::c_void);

    let (event_tx, event_rx) = channel::<Event>();
    clients.lock().unwrap().push(ClientHandle { id: client_id, events: event_tx });

    let mut reader = PipeReader::new(handle);
    let mut writer = PipeWriter::new(handle);

    loop {
        // 1. Drain pending events without blocking
        while let Ok(ev) = event_rx.try_recv() {
            if write_frame(&mut writer, &ev).is_err() {
                break;
            }
        }

        let buffered_bytes = reader.buffer.len() - reader.position;

        // 2. Only query the kernel via PeekNamedPipe if we don't have a 4-byte header in user buffer
        let mut bytes_avail = 0u32;
        if buffered_bytes < 4 {
            let peek_ok = unsafe {
                windows::Win32::System::Pipes::PeekNamedPipe(
                    handle,
                    None,
                    0,
                    None,
                    Some(&mut bytes_avail),
                    None,
                )
            };

            if peek_ok.is_err() {
                // Client closed pipe
                break;
            }
        }

        // 3. Combined check prevents split-header deadlock
        if buffered_bytes + (bytes_avail as usize) >= 4 {
            match read_frame(&mut reader) {
                Ok(Some(req)) => {
                    let (reply_tx, reply_rx) = channel();
                    if command_tx
                        .send(Command::Request { req, reply: reply_tx })
                        .is_err()
                    {
                        break;
                    }
                    unsafe {
                        let _ = SetEvent(HANDLE(wakeup as *mut std::ffi::c_void));
                    }
                    match reply_rx.recv() {
                        Ok(resp) => {
                            if write_frame(&mut writer, &resp).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                    // Immediately loop to handle any pipelined/buffered requests with 0ms latency
                    continue;
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        // 4. Only wait if there are no requests ready to be processed
        match event_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(event) => {
                if write_frame(&mut writer, &event).is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    clients.lock().unwrap().retain(|c| c.id != client_id);
    unsafe {
        let _ = DisconnectNamedPipe(handle);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
}

/// Serialized writer half of a pipe connection.
struct PipeWriter {
    handle: HANDLE,
}

impl PipeWriter {
    fn new(handle: HANDLE) -> Self {
        PipeWriter { handle }
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut written: u32 = 0;
        unsafe {
            WriteFile(self.handle, Some(buf), Some(&mut written), None)
                .map_err(|_| std::io::Error::last_os_error())?;
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_frame<W: Write + ?Sized>(writer: &mut W, value: &impl serde::Serialize) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    writer.write_all(&out)?;
    writer.flush()
}

/// Raw pipe reader with an internal buffer and direct zero-copy filling.
struct PipeReader {
    handle: HANDLE,
    buffer: Vec<u8>,
    position: usize,
}

impl PipeReader {
    fn new(handle: HANDLE) -> Self {
        PipeReader {
            handle,
            buffer: Vec::with_capacity(16 * 1024),
            position: 0,
        }
    }

    fn fill(&mut self, needed: usize) -> std::io::Result<bool> {
        if self.buffer.len() - self.position >= needed {
            return Ok(true);
        }

        // Reclaim already parsed prefix space
        if self.position > 0 {
            self.buffer.drain(..self.position);
            self.position = 0;
        }

        // Prevent memory leaks / unbounded bloat on huge frames
        if self.buffer.capacity() > 256 * 1024 && self.buffer.is_empty() {
            self.buffer.shrink_to(64 * 1024);
        }

        while self.buffer.len() < needed {
            let additional_needed = needed - self.buffer.len();
            // Read in at least 16 KB chunks to minimize ReadFile syscall overhead
            let chunk_size = additional_needed.max(16 * 1024);
            let old_len = self.buffer.len();
            self.buffer.resize(old_len + chunk_size, 0);

            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    Some(&mut self.buffer[old_len..]),
                    Some(&mut read),
                    None,
                )
            };

            // Clamp back to actual bytes received
            self.buffer.truncate(old_len + read as usize);

            if ok.is_err() {
                return Err(std::io::Error::last_os_error());
            }
            if read == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Reads one length-prefixed JSON frame as a raw value, so the caller can
/// tell a response apart from an unsolicited event. `Ok(None)` means the peer
/// closed the connection.
fn read_frame_value(reader: &mut PipeReader) -> std::io::Result<Option<serde_json::Value>> {
    if !reader.fill(4)? {
        // Clean disconnect at frame boundary
        return Ok(None);
    }
    let len = u32::from_le_bytes([
        reader.buffer[reader.position],
        reader.buffer[reader.position + 1],
        reader.buffer[reader.position + 2],
        reader.buffer[reader.position + 3],
    ]) as usize;
    reader.position += 4;

    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    if !reader.fill(len)? {
        // Stream aborted after sending header but before payload
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "unexpected EOF while reading frame body",
        ));
    }
    let body = &reader.buffer[reader.position..reader.position + len];
    reader.position += len;

    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => Ok(Some(value)),
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
    }
}

/// Reads one length-prefixed request frame.
fn read_frame(reader: &mut PipeReader) -> std::io::Result<Option<Request>> {
    let Some(value) = read_frame_value(reader)? else {
        return Ok(None);
    };
    match serde_json::from_value::<Request>(value) {
        Ok(req) => Ok(Some(req)),
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
    }
}

/// Connects to an existing core instance. Returns `None` when no server is up.
#[allow(dead_code)]
pub fn connect_client(timeout_ms: u32) -> Option<Client> {
    let name = HSTRING::from(PIPE_NAME);
    for _ in 0..(timeout_ms / 50).max(1) {
        let handle = unsafe {
            CreateFileW(
                &name,
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        match handle {
            Ok(h) if !h.is_invalid() => {
                return Some(Client { reader: PipeReader::new(h), writer: PipeWriter::new(h) });
            }
            _ => {
                thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    None
}

/// Synchronous client used by the CLI smoke tests.
#[allow(dead_code)]
pub struct Client {
    reader: PipeReader,
    writer: PipeWriter,
}

impl Client {
    /// Sends a request and waits for the matching response, skipping any
    /// unsolicited events that arrive in the meantime.
    #[allow(dead_code)]
    pub fn request(&mut self, req: &Request) -> std::io::Result<Response> {
        write_frame(&mut self.writer, req)?;
        loop {
            let Some(value) = read_frame_value(&mut self.reader)? else {
                return Err(std::io::Error::other("connection closed"));
            };
            if let Ok(resp) = serde_json::from_value::<Response>(value.clone()) {
                if resp.id == req.id {
                    return Ok(resp);
                }
            }
            // Anything else is an event: ignore it and keep waiting.
        }
    }
}

/// Exposed for tests: waits until the pipe instance becomes available.
#[allow(dead_code)]
pub fn wait_for_server(timeout_ms: u32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    while std::time::Instant::now() < deadline {
        if connect_client(50).is_some() {
            return true;
        }
    }
    false
}

#[allow(dead_code)]
fn unused_queue_hint() -> VecDeque<u8> {
    VecDeque::new()
}
