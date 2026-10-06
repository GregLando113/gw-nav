//! Guild Wars fileserver client.
//!
//! The protocol is documented in `docs/fileserver-protocol.md`, which
//! follows the game client's `FcSrv.cpp`. All integers are little endian
//! and every packet starts with a 4-byte header
//! `seq: u8, action: u8, size: u16`, where `size` includes the header.

use std::collections::{HashSet, VecDeque};
use std::hash::BuildHasher;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use super::FileConnError;
use super::decompress::{DecompressError, decompress};
use super::manifest::AssetManifest;

type Result<T> = std::result::Result<T, FileConnError>;

pub const FILESERVER_PORT: u16 = 6112;
/// The game client's second port, tried once 6112 fails everywhere.
pub const FILESERVER_FALLBACK_PORT: u16 = 80;
const FILESERVER_COUNT: u32 = 12;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The game client drops a connection after 30 s without data while files
/// are pending.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// The game client sends a keepalive after 300 s idle. We don't keep a
/// background thread for that, so a connection idle for longer than this is
/// replaced before its next request instead.
const IDLE_RECONNECT: Duration = Duration::from_secs(240);
/// Bytes the server sends before it waits for an ACK. The game client
/// starts with this window and ACKs once this much data is unacknowledged.
const ACK_WINDOW: u32 = 0x4000;
/// Files the game client has requested and not yet received, at most.
const PIPELINE_DEPTH: usize = 32;
/// File ids per request packet. The game client sends a new batch only
/// once this many slots of the pipeline are free.
const REQUEST_BATCH: usize = 16;
/// Times a download is retried on a fresh connection after the connection
/// fails without completing a file.
const RETRIES: u32 = 2;
/// Largest packet the game client accepts, header included.
const MAX_PACKET_SIZE: usize = 0x8004;
/// Largest file size the game client accepts in a FileManifest.
const MAX_FILE_SIZE: u32 = 0x3F0_0000;
/// The sequence byte both sides start with.
const INITIAL_SEQ: u8 = 0xF1;

mod action {
    pub const CLIENT_HELLO: u8 = 0;
    /// Ignored by the game client.
    pub const NOOP_1: u8 = 1;
    pub const SERVER_HELLO: u8 = 2;
    /// Client → server: `n × (u32 file_id, u32 version)`.
    pub const REQUEST: u8 = 3;
    pub const NOT_FOUND: u8 = 4;
    pub const FILE_MANIFEST: u8 = 5;
    pub const FILE_DATA: u8 = 6;
    /// Client → server: `u32` bytes received since the last ACK.
    pub const ACK: u8 = 7;
    /// Ignored by the game client.
    pub const NOOP_9: u8 = 9;
    /// Server → client: `u32 file_id`, the server dropped the requests from
    /// that file on.
    pub const CANCEL: u8 = 15;
}

/// A downloaded file, still compressed.
#[derive(Debug, Clone)]
pub struct RawFile {
    pub file_id: u32,
    pub size_decompressed: u32,
    pub size_compressed: u32,
    /// Checksum reported by the server. Algorithm unknown.
    pub crc: u32,
    pub data: Vec<u8>,
}

impl RawFile {
    pub fn decompress(&self) -> std::result::Result<Vec<u8>, DecompressError> {
        decompress(&self.data, self.size_decompressed as usize)
    }
}

/// What [`FileClient::download_many`] reports.
#[derive(Debug)]
pub enum Fetch {
    /// A chunk of `file_id` arrived. Starts over from 0 if the file is
    /// requested again after a reconnect.
    Bytes { file_id: u32, done: u32, total: u32 },
    /// `file_id` is finished: downloaded, or [`FileConnError::NotFound`] or
    /// [`FileConnError::Cancelled`].
    Done { file_id: u32, result: Result<RawFile> },
}

struct Packet {
    seq: u8,
    action: u8,
    body: Vec<u8>,
}

impl Packet {
    fn unexpected(&self, expected: &'static str) -> FileConnError {
        FileConnError::UnexpectedPacket {
            seq: self.seq,
            action: self.action,
            expected,
        }
    }

    fn u32_at(&self, index: usize) -> Result<u32> {
        let b = self
            .body
            .get(index * 4..index * 4 + 4)
            .ok_or_else(|| FileConnError::Protocol(format!("packet body too short ({} bytes)", self.body.len())))?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Why [`Connection::pipeline`] stopped early. The connection is not used
/// again after any of these.
enum Stop {
    NotFound(u32),
    Cancelled(u32),
    Failed(FileConnError),
}

impl From<FileConnError> for Stop {
    fn from(e: FileConnError) -> Self {
        Stop::Failed(e)
    }
}

/// An open, handshaken connection.
///
/// The first header byte of every packet is a sequence number. The client
/// bumps it when it sends a request or a mid-file ACK, but only once the
/// server has caught up with the previous bump; the server stamps its
/// replies with the latest one it has seen. Each received packet must carry
/// the current sequence or the next one.
struct Connection {
    stream: TcpStream,
    send_seq: u8,
    recv_seq: u8,
    /// FileData bytes received since the last ACK.
    unacked: u32,
    /// Files downloaded on this connection.
    files: u32,
    last_used: Instant,
}

impl Connection {
    fn open(addr: &SocketAddr) -> Result<(Self, [u32; 7])> {
        let mut stream = TcpStream::connect_timeout(addr, CONNECT_TIMEOUT)?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_write_timeout(Some(READ_TIMEOUT))?;
        stream.set_nodelay(true)?;

        // 5 byte prefix, then a ClientHello packet.
        let mut hello = vec![1, 0, 0, 0, 0];
        hello.extend(header(INITIAL_SEQ, action::CLIENT_HELLO, 0x10));
        hello.extend(1u32.to_le_bytes());
        hello.extend(0u32.to_le_bytes());
        hello.extend(0u32.to_le_bytes());
        stream.write_all(&hello)?;

        let mut conn = Self {
            stream,
            send_seq: INITIAL_SEQ,
            recv_seq: INITIAL_SEQ,
            unacked: 0,
            files: 0,
            last_used: Instant::now(),
        };
        let packet = conn.read()?;
        if packet.action != action::SERVER_HELLO {
            return Err(packet.unexpected("ServerHello"));
        }
        let mut manifest = [0; 7];
        for (i, value) in manifest.iter_mut().enumerate() {
            *value = packet.u32_at(i)?;
        }
        Ok((conn, manifest))
    }

    /// Bump the sequence unless the server hasn't caught up yet.
    fn bump_seq(&mut self) {
        if self.send_seq == self.recv_seq {
            self.send_seq = self.send_seq.wrapping_add(1);
        }
    }

    fn send_ack(&mut self) -> Result<()> {
        let mut packet = header(self.send_seq, action::ACK, 8).to_vec();
        packet.extend(self.unacked.to_le_bytes());
        self.unacked = 0;
        self.stream.write_all(&packet)?;
        Ok(())
    }

    /// Request `file_ids` in one packet, with any unacknowledged bytes ACKed
    /// in front of it.
    fn send_request(&mut self, file_ids: &[u32]) -> Result<()> {
        self.bump_seq();
        let mut packets = Vec::with_capacity(12 + 8 * file_ids.len());
        if self.unacked > 0 {
            packets.extend(header(self.send_seq, action::ACK, 8));
            packets.extend(self.unacked.to_le_bytes());
            self.unacked = 0;
        }
        packets.extend(header(self.send_seq, action::REQUEST, (4 + 8 * file_ids.len()) as u16));
        for id in file_ids {
            packets.extend(id.to_le_bytes());
            packets.extend(0u32.to_le_bytes()); // version 0 = full file, not a delta
        }
        self.stream.write_all(&packets)?;
        Ok(())
    }

    /// Read the next packet, checking its sequence and skipping the packets
    /// the game client ignores.
    fn read(&mut self) -> Result<Packet> {
        loop {
            let packet = read_packet(&mut self.stream)?;
            if packet.seq != self.recv_seq {
                if packet.seq != self.recv_seq.wrapping_add(1) {
                    return Err(FileConnError::Protocol(format!(
                        "packet sequence {:#04x}, expected {:#04x} or the next one",
                        packet.seq, self.recv_seq
                    )));
                }
                self.recv_seq = packet.seq;
            }
            if !matches!(packet.action, action::NOOP_1 | action::NOOP_9) {
                return Ok(packet);
            }
        }
    }

    /// Download files from `queue` until it is empty, keeping the pipeline
    /// full. If it stops early, the files that were in flight are put back
    /// at the front of `queue` (except one that was not found).
    fn pipeline(&mut self, queue: &mut VecDeque<u32>, on_event: &mut impl FnMut(Fetch)) -> std::result::Result<(), Stop> {
        let mut in_flight = Vec::new();
        let result = self.run_pipeline(queue, &mut in_flight, on_event);
        if let Err(Stop::NotFound(id)) = &result {
            in_flight.retain(|f| f != id);
        }
        for &id in in_flight.iter().rev() {
            queue.push_front(id);
        }
        result
    }

    fn run_pipeline(
        &mut self,
        queue: &mut VecDeque<u32>,
        in_flight: &mut Vec<u32>,
        on_event: &mut impl FnMut(Fetch),
    ) -> std::result::Result<(), Stop> {
        self.dispatch(queue, in_flight)?;
        while !in_flight.is_empty() {
            let file = self.receive_file(in_flight, on_event)?;
            in_flight.retain(|&f| f != file.file_id);
            self.files += 1;
            on_event(Fetch::Done { file_id: file.file_id, result: Ok(file) });
            self.dispatch(queue, in_flight)?;
            // With files still pending, the rest of this file is ACKed with
            // the next window or the next request. Otherwise ACK it now.
            if in_flight.is_empty() {
                self.send_ack()?;
            } else if self.unacked >= ACK_WINDOW {
                self.bump_seq();
                self.send_ack()?;
            }
        }
        Ok(())
    }

    /// Request more files once a full batch of the pipeline is free.
    fn dispatch(&mut self, queue: &mut VecDeque<u32>, in_flight: &mut Vec<u32>) -> Result<()> {
        while !queue.is_empty() && PIPELINE_DEPTH - in_flight.len() >= REQUEST_BATCH {
            let start = in_flight.len();
            in_flight.extend(queue.drain(..REQUEST_BATCH.min(queue.len())));
            let batch = in_flight[start..].to_vec();
            self.send_request(&batch)?;
        }
        Ok(())
    }

    /// Receive the next file the server sends, which must be one of
    /// `in_flight`.
    fn receive_file(&mut self, in_flight: &[u32], on_event: &mut impl FnMut(Fetch)) -> std::result::Result<RawFile, Stop> {
        let requested = |id: u32| -> Result<u32> {
            if in_flight.contains(&id) {
                Ok(id)
            } else {
                Err(FileConnError::Protocol(format!("server sent file {id}, which was not requested")))
            }
        };
        let packet = self.read()?;
        match packet.action {
            action::NOT_FOUND => {
                let id = packet.u32_at(0).unwrap_or(in_flight[0]);
                return Err(Stop::NotFound(requested(id)?));
            }
            action::CANCEL => return Err(Stop::Cancelled(requested(packet.u32_at(0)?)?)),
            action::FILE_MANIFEST => {}
            _ => return Err(packet.unexpected("FileManifest").into()),
        }
        let file_id = requested(packet.u32_at(0)?)?;
        let mut file = RawFile {
            file_id,
            size_decompressed: packet.u32_at(1)?,
            size_compressed: packet.u32_at(2)?,
            crc: packet.u32_at(3)?,
            data: Vec::new(),
        };
        for size in [file.size_decompressed, file.size_compressed] {
            if size == 0 || size > MAX_FILE_SIZE {
                return Err(FileConnError::Protocol(format!("file {file_id} has bad size {size}")).into());
            }
        }
        let total = file.size_compressed;
        file.data.reserve(total as usize);

        // ACK every window of data with the number of bytes received since
        // the last ACK. The ACKs must add up to exactly the data received.
        while (file.data.len() as u32) < total {
            let packet = self.read()?;
            match packet.action {
                action::FILE_DATA => {}
                action::CANCEL => return Err(Stop::Cancelled(requested(packet.u32_at(0)?)?)),
                _ => return Err(packet.unexpected("FileData").into()),
            }
            let received = file.data.len() + packet.body.len();
            if received > total as usize {
                return Err(FileConnError::Protocol(format!("file {file_id} sent {received} bytes, expected {total}")).into());
            }
            file.data.extend_from_slice(&packet.body);
            self.unacked += packet.body.len() as u32;
            on_event(Fetch::Bytes { file_id, done: received as u32, total });
            if received < total as usize && self.unacked >= ACK_WINDOW {
                self.bump_seq();
                self.send_ack()?;
            }
        }
        Ok(file)
    }
}

/// Connection to one fileserver.
///
/// After a NotFound or Cancel reply, or any error, the connection is closed
/// and reopened for the next request.
pub struct FileClient {
    addr: SocketAddr,
    /// `None` when the connection must be re-established before the next
    /// request.
    conn: Option<Connection>,
    manifest: [u32; 7],
    idle_reconnect: Duration,
}

impl FileClient {
    /// Connect to the first responsive official fileserver
    /// (`file1..file12.arenanetworks.com`), starting at a random one so that
    /// several connections spread across the servers, as the game client's
    /// do. Every server is tried on port 6112 first, then on port 80.
    pub fn connect() -> Result<Self> {
        let start = std::collections::hash_map::RandomState::new().hash_one(0u8) as u32 % FILESERVER_COUNT;
        let mut errors = Vec::new();
        for port in [FILESERVER_PORT, FILESERVER_FALLBACK_PORT] {
            for k in 0..FILESERVER_COUNT {
                let host = format!("file{}.arenanetworks.com", (start + k) % FILESERVER_COUNT + 1);
                match Self::connect_to((host.as_str(), port)) {
                    Ok(client) => return Ok(client),
                    Err(e) => errors.push(format!("{host}:{port}: {e}")),
                }
            }
        }
        Err(FileConnError::NoServer(errors.join("; ")))
    }

    /// Connect to a specific server and perform the handshake.
    pub fn connect_to(addr: impl ToSocketAddrs) -> Result<Self> {
        let mut last_err = None;
        for addr in addr.to_socket_addrs()? {
            let mut client = Self {
                addr,
                conn: None,
                manifest: [0; 7],
                idle_reconnect: IDLE_RECONNECT,
            };
            match client.reconnect() {
                Ok(()) => return Ok(client),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::other("address resolved to nothing").into()))
    }

    /// Open a fresh connection to `self.addr` and do the handshake.
    fn reconnect(&mut self) -> Result<()> {
        self.conn = None;
        let (conn, manifest) = Connection::open(&self.addr)?;
        self.conn = Some(conn);
        self.manifest = manifest;
        Ok(())
    }

    /// File ids sent by the server on connect (index 1 = asset manifest,
    /// index 6 = Gw.exe).
    pub fn manifest(&self) -> [u32; 7] {
        self.manifest
    }

    /// File id of the current asset manifest. It changes with game updates,
    /// so it also works as a cache key.
    pub fn asset_manifest_id(&self) -> u32 {
        self.manifest[1]
    }

    /// Download and parse the asset manifest.
    pub fn download_asset_manifest(&mut self, progress: impl FnMut(u32, u32)) -> Result<AssetManifest> {
        let data = self.download(self.asset_manifest_id(), progress)?;
        Ok(AssetManifest::parse(&data)?)
    }

    /// Download a file and decompress it.
    pub fn download(&mut self, file_id: u32, progress: impl FnMut(u32, u32)) -> Result<Vec<u8>> {
        Ok(self.download_raw(file_id, progress)?.decompress()?)
    }

    /// Download a file without decompressing it. `progress` is called with
    /// `(bytes_received, bytes_total)` after every chunk. Retries like
    /// [`Self::download_many`].
    pub fn download_raw(&mut self, file_id: u32, mut progress: impl FnMut(u32, u32)) -> Result<RawFile> {
        let mut file = None;
        self.download_many(&[file_id], |event| match event {
            Fetch::Bytes { done, total, .. } => progress(done, total),
            Fetch::Done { result, .. } => file = Some(result),
        })?;
        file.expect("download_many reports every file")
    }

    /// Download several files over this connection, with up to 32 requests
    /// in flight as the game client does. `on_event` reports progress and
    /// each distinct file id once, in the order the files arrive.
    ///
    /// Reconnects as needed. After a NotFound, a Cancel or a failure the
    /// files that were in flight are requested again on a new connection.
    /// A file the server cancels twice is reported as
    /// [`FileConnError::Cancelled`]. If the connection fails 3 times in a
    /// row without finishing a file, this returns that error, and the files
    /// not yet reported were not downloaded.
    pub fn download_many(&mut self, file_ids: &[u32], mut on_event: impl FnMut(Fetch)) -> Result<()> {
        let mut seen = HashSet::new();
        let mut queue: VecDeque<u32> = file_ids.iter().copied().filter(|&id| seen.insert(id)).collect();
        if self.conn.as_ref().is_some_and(|c| c.last_used.elapsed() > self.idle_reconnect) {
            self.conn = None;
        }
        let mut cancelled = HashSet::new();
        let mut failures = 0;
        while !queue.is_empty() {
            if self.conn.is_none()
                && let Err(e) = self.reconnect()
            {
                failures += 1;
                if failures > RETRIES {
                    return Err(e);
                }
                continue;
            }
            let conn = self.conn.as_mut().expect("connected above");
            let files_before = conn.files;
            let result = conn.pipeline(&mut queue, &mut on_event);
            conn.last_used = Instant::now();
            if conn.files > files_before {
                failures = 0;
            }
            let Err(stop) = result else { break };
            // NotFound leaves the server about to reset, Cancel drops the
            // other requests, and any error leaves the stream in an unknown
            // state.
            self.conn = None;
            match stop {
                Stop::NotFound(id) => on_event(Fetch::Done { file_id: id, result: Err(FileConnError::NotFound(id)) }),
                Stop::Cancelled(id) => {
                    if !cancelled.insert(id) {
                        queue.retain(|&f| f != id);
                        on_event(Fetch::Done { file_id: id, result: Err(FileConnError::Cancelled(id)) });
                    }
                }
                Stop::Failed(e) => {
                    failures += 1;
                    if failures > RETRIES {
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }
}

fn read_packet(stream: &mut TcpStream) -> Result<Packet> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    let size = u16::from_le_bytes([head[2], head[3]]) as usize;
    if !(4..=MAX_PACKET_SIZE).contains(&size) {
        return Err(FileConnError::Protocol(format!("bad packet size {size}")));
    }
    let mut body = vec![0u8; size - 4];
    stream.read_exact(&mut body)?;
    Ok(Packet {
        seq: head[0],
        action: head[1],
        body,
    })
}

fn header(seq: u8, action: u8, size: u16) -> [u8; 4] {
    let s = size.to_le_bytes();
    [seq, action, s[0], s[1]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    const HELLO_BYTES: [u8; 21] = [
        1, 0, 0, 0, 0, 0xF1, 0x00, 0x10, 0x00, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    fn packet(seq: u8, action: u8, body: &[u8]) -> Vec<u8> {
        let mut p = header(seq, action, (body.len() + 4) as u16).to_vec();
        p.extend_from_slice(body);
        p
    }

    fn u32s(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn expect_bytes(stream: &mut TcpStream, expected: &[u8]) {
        let mut buf = vec![0u8; expected.len()];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(buf, expected);
    }

    fn expect_requests(stream: &mut TcpStream, seq: u8, file_ids: &[u32]) {
        let body: Vec<u32> = file_ids.iter().flat_map(|&id| [id, 0]).collect();
        expect_bytes(stream, &packet(seq, action::REQUEST, &u32s(&body)));
    }

    fn expect_request(stream: &mut TcpStream, seq: u8, file_id: u32) {
        expect_requests(stream, seq, &[file_id]);
    }

    fn expect_ack(stream: &mut TcpStream, seq: u8, bytes: u32) {
        expect_bytes(stream, &packet(seq, action::ACK, &u32s(&[bytes])));
    }

    /// Accept a connection and do the server side of the handshake.
    fn accept(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().unwrap();
        expect_bytes(&mut stream, &HELLO_BYTES);
        let hello = packet(INITIAL_SEQ, action::SERVER_HELLO, &u32s(&[10, 11, 12, 13, 14, 15, 16]));
        stream.write_all(&hello).unwrap();
        stream
    }

    /// Send a stored-uncompressed file in one chunk.
    fn send_small_file(stream: &mut TcpStream, seq: u8, file_id: u32, data: &[u8]) {
        let len = data.len() as u32;
        let manifest = packet(seq, action::FILE_MANIFEST, &u32s(&[file_id, len, len, 0]));
        stream.write_all(&manifest).unwrap();
        stream.write_all(&packet(seq, action::FILE_DATA, data)).unwrap();
    }

    /// Run a fake fileserver on a thread; `script` accepts connections.
    fn serve(script: impl FnOnce(&TcpListener) + Send + 'static) -> (FileClient, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || script(&listener));
        (FileClient::connect_to(addr).unwrap(), server)
    }

    /// A fake fileserver whose every connection (the first and each retry)
    /// answers a request for file 7 with `reply`.
    fn serve_failing(reply: Vec<u8>) -> (FileClient, JoinHandle<()>) {
        serve(move |l| {
            for _ in 0..=RETRIES {
                let mut s = accept(l);
                expect_request(&mut s, 0xF2, 7);
                s.write_all(&reply).unwrap();
            }
        })
    }

    /// `n` bytes of a 4-byte file id pattern, so files are distinguishable.
    fn file_bytes(id: u32, n: usize) -> Vec<u8> {
        id.to_le_bytes().iter().copied().cycle().take(n).collect()
    }

    #[test]
    fn handshake_reads_manifest() {
        let (client, server) = serve(|l| {
            accept(l);
        });
        assert_eq!(client.manifest(), [10, 11, 12, 13, 14, 15, 16]);
        server.join().unwrap();
    }

    #[test]
    fn multi_chunk_download_acks_each_window() {
        // 5 chunks: 8K, 8K (ACK 16K), 8K, 8K (ACK 16K), 7232 (ACK the rest).
        let data: Vec<u8> = (0..=255u8).cycle().take(40_000).collect();
        let server_data = data.clone();
        let (mut client, server) = serve(move |l| {
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 42);
            let manifest = packet(0xF2, action::FILE_MANIFEST, &u32s(&[42, 50_000, 40_000, 0xDEAD]));
            s.write_all(&manifest).unwrap();
            let mut seq = 0xF2;
            for (i, chunk) in server_data.chunks(8192).enumerate() {
                s.write_all(&packet(seq, action::FILE_DATA, chunk)).unwrap();
                if i % 2 == 1 {
                    // Mid-file ACKs bump the sequence; the server follows.
                    seq += 1;
                    expect_ack(&mut s, seq, 16_384);
                }
            }
            expect_ack(&mut s, seq, 40_000 - 4 * 8192);
        });

        let mut seen = Vec::new();
        let raw = client.download_raw(42, |done, total| seen.push((done, total))).unwrap();
        assert_eq!(raw.file_id, 42);
        assert_eq!(raw.size_decompressed, 50_000);
        assert_eq!(raw.size_compressed, 40_000);
        assert_eq!(raw.crc, 0xDEAD);
        assert_eq!(raw.data, data);
        let expected: Vec<_> = [8192, 16_384, 24_576, 32_768, 40_000].iter().map(|&n| (n, 40_000)).collect();
        assert_eq!(seen, expected);
        server.join().unwrap();
    }

    #[test]
    fn later_requests_bump_the_sequence() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 1);
            send_small_file(&mut s, 0xF2, 1, b"one!");
            // The end-of-file ACK does not bump.
            expect_ack(&mut s, 0xF2, 4);
            expect_request(&mut s, 0xF3, 2);
            send_small_file(&mut s, 0xF3, 2, b"two!");
            expect_ack(&mut s, 0xF3, 4);
        });
        assert_eq!(client.download(1, |_, _| {}).unwrap(), b"one!");
        assert_eq!(client.download(2, |_, _| {}).unwrap(), b"two!");
        server.join().unwrap();
    }

    #[test]
    fn pipeline_batches_requests_and_defers_acks() {
        // 40 files of 1000 bytes: 32 requested up front in two batches of
        // 16; the remaining 8 once 16 slots are free.
        let ids: Vec<u32> = (100..140).collect();
        let server_ids = ids.clone();
        let (mut client, server) = serve(move |l| {
            let mut s = accept(l);
            // The second batch goes out before the server has replied, so
            // it does not bump the sequence.
            expect_requests(&mut s, 0xF2, &server_ids[..16]);
            expect_requests(&mut s, 0xF2, &server_ids[16..32]);
            // Each sequence bump is followed by the server.
            let (mut seq, mut unacked) = (0xF2, 0);
            for (n, &id) in server_ids.iter().enumerate() {
                send_small_file(&mut s, seq, id, &file_bytes(id, 1000));
                unacked += 1000;
                if n == 15 {
                    // 16 slots free: the next batch goes out, with the
                    // 16000 bytes not yet ACKed in front of it.
                    seq += 1;
                    expect_ack(&mut s, seq, unacked);
                    expect_requests(&mut s, seq, &server_ids[32..]);
                    unacked = 0;
                } else if n < 39 && unacked >= 16_384 {
                    // Files pending: ACK a full window, not each file.
                    seq += 1;
                    expect_ack(&mut s, seq, unacked);
                    unacked = 0;
                }
            }
            // Nothing pending after the last file: ACK the rest, no bump.
            expect_ack(&mut s, seq, unacked);
        });

        let mut done = Vec::new();
        client
            .download_many(&ids, |event| {
                if let Fetch::Done { file_id, result } = event {
                    assert_eq!(result.unwrap().data, file_bytes(file_id, 1000));
                    done.push(file_id);
                }
            })
            .unwrap();
        assert_eq!(done, ids);
        server.join().unwrap();
    }

    #[test]
    fn duplicate_ids_are_requested_once() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_requests(&mut s, 0xF2, &[1, 2]);
            send_small_file(&mut s, 0xF2, 1, b"one!");
            send_small_file(&mut s, 0xF2, 2, b"two!");
            expect_ack(&mut s, 0xF2, 8);
        });
        let mut done = Vec::new();
        client
            .download_many(&[1, 2, 1], |event| {
                if let Fetch::Done { file_id, .. } = event {
                    done.push(file_id);
                }
            })
            .unwrap();
        assert_eq!(done, [1, 2]);
        server.join().unwrap();
    }

    #[test]
    fn ignored_packets_are_skipped() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 1);
            s.write_all(&packet(0xF2, action::NOOP_9, &[])).unwrap();
            let manifest = packet(0xF2, action::FILE_MANIFEST, &u32s(&[1, 4, 4, 0]));
            s.write_all(&manifest).unwrap();
            s.write_all(&packet(0xF2, action::NOOP_1, &u32s(&[0]))).unwrap();
            s.write_all(&packet(0xF2, action::FILE_DATA, b"ffna")).unwrap();
            expect_ack(&mut s, 0xF2, 4);
        });
        assert_eq!(client.download(1, |_, _| {}).unwrap(), b"ffna");
        server.join().unwrap();
    }

    #[test]
    fn not_found_reconnects() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 1);
            s.write_all(&packet(0xF2, action::NOT_FOUND, &u32s(&[1]))).unwrap();
            // The client must open a new connection, starting over at 0xF2.
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 2);
            send_small_file(&mut s, 0xF2, 2, b"ffna");
            expect_ack(&mut s, 0xF2, 4);
        });
        assert!(matches!(
            client.download_raw(1, |_, _| {}),
            Err(FileConnError::NotFound(1))
        ));
        assert_eq!(client.download(2, |_, _| {}).unwrap(), b"ffna");
        server.join().unwrap();
    }

    #[test]
    fn not_found_in_a_pipeline_requeues_the_rest() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_requests(&mut s, 0xF2, &[1, 2, 3]);
            send_small_file(&mut s, 0xF2, 1, b"one!");
            s.write_all(&packet(0xF2, action::NOT_FOUND, &u32s(&[2]))).unwrap();
            let mut s = accept(l);
            expect_requests(&mut s, 0xF2, &[3]);
            send_small_file(&mut s, 0xF2, 3, b"thr!");
            expect_ack(&mut s, 0xF2, 4);
        });
        let mut done = Vec::new();
        client
            .download_many(&[1, 2, 3], |event| {
                if let Fetch::Done { file_id, result } = event {
                    done.push((file_id, result.is_ok()));
                }
            })
            .unwrap();
        assert_eq!(done, [(1, true), (2, false), (3, true)]);
        server.join().unwrap();
    }

    #[test]
    fn cancel_retries_on_a_new_connection() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 3);
            s.write_all(&packet(0xF2, action::CANCEL, &u32s(&[3]))).unwrap();
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 3);
            send_small_file(&mut s, 0xF2, 3, b"ffna");
            expect_ack(&mut s, 0xF2, 4);
        });
        assert_eq!(client.download(3, |_, _| {}).unwrap(), b"ffna");
        server.join().unwrap();
    }

    #[test]
    fn second_cancel_is_reported() {
        let (mut client, server) = serve(|l| {
            for _ in 0..2 {
                let mut s = accept(l);
                expect_request(&mut s, 0xF2, 3);
                s.write_all(&packet(0xF2, action::CANCEL, &u32s(&[3]))).unwrap();
            }
        });
        assert!(matches!(
            client.download_raw(3, |_, _| {}),
            Err(FileConnError::Cancelled(3))
        ));
        server.join().unwrap();
    }

    #[test]
    fn reused_connection_retries_after_io_error() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 1);
            send_small_file(&mut s, 0xF2, 1, b"one!");
            expect_ack(&mut s, 0xF2, 4);
            drop(s); // server closes the connection while idle
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 2);
            send_small_file(&mut s, 0xF2, 2, b"two!");
            expect_ack(&mut s, 0xF2, 4);
        });
        assert_eq!(client.download(1, |_, _| {}).unwrap(), b"one!");
        assert_eq!(client.download(2, |_, _| {}).unwrap(), b"two!");
        server.join().unwrap();
    }

    #[test]
    fn idle_connection_is_replaced() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 1);
            send_small_file(&mut s, 0xF2, 1, b"one!");
            expect_ack(&mut s, 0xF2, 4);
            // No request arrives on the old connection.
            let mut s = accept(l);
            expect_request(&mut s, 0xF2, 2);
            send_small_file(&mut s, 0xF2, 2, b"two!");
            expect_ack(&mut s, 0xF2, 4);
        });
        assert_eq!(client.download(1, |_, _| {}).unwrap(), b"one!");
        client.idle_reconnect = Duration::ZERO;
        assert_eq!(client.download(2, |_, _| {}).unwrap(), b"two!");
        server.join().unwrap();
    }

    #[test]
    fn unexpected_packet_errors() {
        let (mut client, server) = serve_failing(packet(0xF2, 11, &[]));
        let err = client.download_raw(7, |_, _| {}).unwrap_err();
        assert!(matches!(
            err,
            FileConnError::UnexpectedPacket {
                seq: 0xF2,
                action: 11,
                ..
            }
        ));
        server.join().unwrap();
    }

    #[test]
    fn bad_sequence_errors() {
        let (mut client, server) = serve_failing(packet(0xF4, action::FILE_MANIFEST, &u32s(&[7, 4, 4, 0])));
        assert!(matches!(
            client.download_raw(7, |_, _| {}),
            Err(FileConnError::Protocol(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn overlong_data_errors() {
        let mut reply = packet(0xF2, action::FILE_MANIFEST, &u32s(&[7, 4, 4, 0]));
        reply.extend(packet(0xF2, action::FILE_DATA, b"too long"));
        let (mut client, server) = serve_failing(reply);
        assert!(matches!(
            client.download_raw(7, |_, _| {}),
            Err(FileConnError::Protocol(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn mismatched_file_id_errors() {
        let (mut client, server) = serve_failing(packet(0xF2, action::FILE_MANIFEST, &u32s(&[8, 4, 4, 0])));
        assert!(matches!(
            client.download_raw(7, |_, _| {}),
            Err(FileConnError::Protocol(_))
        ));
        server.join().unwrap();
    }
}
