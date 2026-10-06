//! Guild Wars fileserver client.
//!
//! Protocol notes (from `references/file/cli.py`): all integers are little
//! endian and every packet starts with a 4-byte header
//! `stage: u8, action: u8, size: u16`, where `size` includes the header.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use super::FileConnError;
use super::decompress::{DecompressError, decompress};
use super::manifest::AssetManifest;

type Result<T> = std::result::Result<T, FileConnError>;

pub const FILESERVER_PORT: u16 = 6112;
const FILESERVER_COUNT: u32 = 12;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Sent with every RequestMore packet, as in cli.py.
const DATA_RATE: u32 = 0x4000;

mod stage {
    pub const HELLO: u8 = 0xF1;
    pub const REQUEST: u8 = 0xF2;
    pub const MORE: u8 = 0xF3;
}

mod action {
    pub const CLIENT_HELLO: u8 = 0;
    pub const SERVER_HELLO: u8 = 2;
    pub const REQUEST: u8 = 3;
    pub const NOT_FOUND: u8 = 4;
    pub const FILE_MANIFEST: u8 = 5;
    pub const FILE_DATA: u8 = 6;
    pub const REQUEST_MORE: u8 = 7;
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

struct Packet {
    stage: u8,
    action: u8,
    body: Vec<u8>,
}

impl Packet {
    fn unexpected(&self, expected: &'static str) -> FileConnError {
        FileConnError::UnexpectedPacket {
            stage: self.stage,
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

/// Connection to one fileserver.
///
/// Observed server behavior:
/// - The first request on a connection is sent with stage `REQUEST` (0xF2),
///   later ones with stage `MORE` (0xF3). Otherwise the server resets.
/// - After a NotFound reply the server resets the connection on the next
///   request, so the client reconnects instead.
pub struct FileClient {
    addr: SocketAddr,
    /// `None` when the connection must be re-established before the next
    /// request.
    stream: Option<TcpStream>,
    /// Requests sent on the current connection.
    requests: u32,
    manifest: [u32; 7],
}

impl FileClient {
    /// Connect to the first responsive official fileserver
    /// (`file1..file12.arenanetworks.com`).
    pub fn connect() -> Result<Self> {
        let mut errors = Vec::new();
        for i in 1..=FILESERVER_COUNT {
            let host = format!("file{i}.arenanetworks.com");
            match Self::connect_to((host.as_str(), FILESERVER_PORT)) {
                Ok(client) => return Ok(client),
                Err(e) => errors.push(format!("{host}: {e}")),
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
                stream: None,
                requests: 0,
                manifest: [0; 7],
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
        self.stream = None;
        self.requests = 0;
        let mut stream = TcpStream::connect_timeout(&self.addr, CONNECT_TIMEOUT)?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_write_timeout(Some(READ_TIMEOUT))?;
        stream.set_nodelay(true)?;

        // 5 byte prefix, then a ClientHello packet.
        let mut hello = vec![1, 0, 0, 0, 0];
        hello.extend(header(stage::HELLO, action::CLIENT_HELLO, 0x10));
        hello.extend(1u32.to_le_bytes());
        hello.extend(0u32.to_le_bytes());
        hello.extend(0u32.to_le_bytes());
        stream.write_all(&hello)?;

        let packet = read_packet(&mut stream)?;
        if packet.stage != stage::HELLO || packet.action != action::SERVER_HELLO {
            return Err(packet.unexpected("ServerHello"));
        }
        for (i, value) in self.manifest.iter_mut().enumerate() {
            *value = packet.u32_at(i)?;
        }
        self.stream = Some(stream);
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
    /// `(bytes_received, bytes_total)` after every chunk.
    ///
    /// Reconnects if needed, and retries once if a reused connection fails
    /// with an I/O error (e.g. the server closed it while idle).
    pub fn download_raw(&mut self, file_id: u32, mut progress: impl FnMut(u32, u32)) -> Result<RawFile> {
        let reused = self.stream.is_some() && self.requests > 0;
        match self.try_download(file_id, &mut progress) {
            Err(FileConnError::Io(_)) if reused => self.try_download(file_id, &mut progress),
            result => result,
        }
    }

    fn try_download(&mut self, file_id: u32, progress: &mut impl FnMut(u32, u32)) -> Result<RawFile> {
        if self.stream.is_none() {
            self.reconnect()?;
        }
        let stream = self.stream.as_mut().expect("connected above");
        let first = self.requests == 0;
        self.requests += 1;
        let result = request_file(stream, file_id, first, progress);
        if result.is_err() {
            // NotFound leaves the server about to reset; any other error
            // leaves the stream in an unknown state.
            self.stream = None;
        }
        result
    }
}

fn request_file(
    stream: &mut TcpStream,
    file_id: u32,
    first: bool,
    progress: &mut impl FnMut(u32, u32),
) -> Result<RawFile> {
    let request_stage = if first { stage::REQUEST } else { stage::MORE };
    let mut request = header(request_stage, action::REQUEST, 0xC).to_vec();
    request.extend(file_id.to_le_bytes());
    request.extend(0u32.to_le_bytes()); // version 0 = full file, not a delta
    stream.write_all(&request)?;

    let packet = read_packet(stream)?;
    match packet.action {
        action::NOT_FOUND => return Err(FileConnError::NotFound(file_id)),
        action::FILE_MANIFEST => {}
        _ => return Err(packet.unexpected("FileManifest")),
    }
    let received_id = packet.u32_at(0)?;
    if received_id != file_id {
        return Err(FileConnError::Protocol(format!(
            "requested file {file_id} but server sent {received_id}"
        )));
    }
    let mut file = RawFile {
        file_id,
        size_decompressed: packet.u32_at(1)?,
        size_compressed: packet.u32_at(2)?,
        crc: packet.u32_at(3)?,
        data: Vec::new(),
    };
    let total = file.size_compressed;
    file.data.reserve(total as usize);

    loop {
        let packet = read_packet(stream)?;
        if packet.action != action::FILE_DATA {
            return Err(packet.unexpected("FileData"));
        }
        file.data.extend_from_slice(&packet.body);
        let received = file.data.len().min(total as usize) as u32;
        progress(received, total);
        if received >= total {
            break;
        }
        let mut more = header(stage::MORE, action::REQUEST_MORE, 0x8).to_vec();
        more.extend(DATA_RATE.to_le_bytes());
        stream.write_all(&more)?;
    }
    file.data.truncate(total as usize);
    // Acknowledge the completed file. The server limits unacknowledged
    // data per connection (about 120 KB observed) and stops sending once
    // it is reached, so without this every few small files stall until the
    // read timeout.
    let mut complete = header(stage::MORE, action::REQUEST_MORE, 0x8).to_vec();
    complete.extend(total.to_le_bytes());
    stream.write_all(&complete)?;
    Ok(file)
}

fn read_packet(stream: &mut TcpStream) -> Result<Packet> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    let size = u16::from_le_bytes([head[2], head[3]]) as usize;
    if size < 4 {
        return Err(FileConnError::Protocol(format!("packet size {size} is smaller than its header")));
    }
    let mut body = vec![0u8; size - 4];
    stream.read_exact(&mut body)?;
    Ok(Packet {
        stage: head[0],
        action: head[1],
        body,
    })
}

fn header(stage: u8, action: u8, size: u16) -> [u8; 4] {
    let s = size.to_le_bytes();
    [stage, action, s[0], s[1]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    const HELLO_BYTES: [u8; 21] = [
        1, 0, 0, 0, 0, 0xF1, 0x00, 0x10, 0x00, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    fn packet(stage: u8, action: u8, body: &[u8]) -> Vec<u8> {
        let mut p = header(stage, action, (body.len() + 4) as u16).to_vec();
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

    fn expect_request(stream: &mut TcpStream, stage: u8, file_id: u32) {
        expect_bytes(stream, &packet(stage, action::REQUEST, &u32s(&[file_id, 0])));
    }

    fn expect_more(stream: &mut TcpStream) {
        expect_bytes(stream, &packet(stage::MORE, action::REQUEST_MORE, &u32s(&[DATA_RATE])));
    }

    fn expect_complete(stream: &mut TcpStream, len: u32) {
        expect_bytes(stream, &packet(stage::MORE, action::REQUEST_MORE, &u32s(&[len])));
    }

    /// Accept a connection and do the server side of the handshake.
    fn accept(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().unwrap();
        expect_bytes(&mut stream, &HELLO_BYTES);
        let hello = packet(stage::HELLO, action::SERVER_HELLO, &u32s(&[10, 11, 12, 13, 14, 15, 16]));
        stream.write_all(&hello).unwrap();
        stream
    }

    /// Send a stored-uncompressed file in one chunk.
    fn send_small_file(stream: &mut TcpStream, stage: u8, file_id: u32, data: &[u8]) {
        let len = data.len() as u32;
        let manifest = packet(stage, action::FILE_MANIFEST, &u32s(&[file_id, len, len, 0]));
        stream.write_all(&manifest).unwrap();
        stream.write_all(&packet(stage, action::FILE_DATA, data)).unwrap();
    }

    /// Run a fake fileserver on a thread; `script` accepts connections.
    fn serve(script: impl FnOnce(&TcpListener) + Send + 'static) -> (FileClient, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || script(&listener));
        (FileClient::connect_to(addr).unwrap(), server)
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
    fn multi_chunk_download() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let server_data = data.clone();
        let (mut client, server) = serve(move |l| {
            let mut s = accept(l);
            expect_request(&mut s, stage::REQUEST, 42);
            let manifest = packet(stage::REQUEST, action::FILE_MANIFEST, &u32s(&[42, 5000, 1000, 0xDEAD]));
            s.write_all(&manifest).unwrap();
            for (i, chunk) in server_data.chunks(400).enumerate() {
                if i > 0 {
                    expect_more(&mut s);
                }
                let stage = if i == 0 { stage::REQUEST } else { stage::MORE };
                s.write_all(&packet(stage, action::FILE_DATA, chunk)).unwrap();
            }
            expect_complete(&mut s, 1000);
        });

        let mut seen = Vec::new();
        let raw = client.download_raw(42, |done, total| seen.push((done, total))).unwrap();
        assert_eq!(raw.file_id, 42);
        assert_eq!(raw.size_decompressed, 5000);
        assert_eq!(raw.size_compressed, 1000);
        assert_eq!(raw.crc, 0xDEAD);
        assert_eq!(raw.data, data);
        assert_eq!(seen, vec![(400, 1000), (800, 1000), (1000, 1000)]);
        server.join().unwrap();
    }

    #[test]
    fn later_requests_use_more_stage() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, stage::REQUEST, 1);
            send_small_file(&mut s, stage::REQUEST, 1, b"one!");
            expect_complete(&mut s, 4);
            expect_request(&mut s, stage::MORE, 2);
            send_small_file(&mut s, stage::MORE, 2, b"two!");
            expect_complete(&mut s, 4);
        });
        assert_eq!(client.download(1, |_, _| {}).unwrap(), b"one!");
        assert_eq!(client.download(2, |_, _| {}).unwrap(), b"two!");
        server.join().unwrap();
    }

    #[test]
    fn not_found_reconnects() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, stage::REQUEST, 1);
            s.write_all(&packet(stage::REQUEST, action::NOT_FOUND, &u32s(&[1]))).unwrap();
            // The client must open a new connection, starting over at stage REQUEST.
            let mut s = accept(l);
            expect_request(&mut s, stage::REQUEST, 2);
            send_small_file(&mut s, stage::REQUEST, 2, b"ffna");
            expect_complete(&mut s, 4);
        });
        assert!(matches!(
            client.download_raw(1, |_, _| {}),
            Err(FileConnError::NotFound(1))
        ));
        assert_eq!(client.download(2, |_, _| {}).unwrap(), b"ffna");
        server.join().unwrap();
    }

    #[test]
    fn reused_connection_retries_after_io_error() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, stage::REQUEST, 1);
            send_small_file(&mut s, stage::REQUEST, 1, b"one!");
            expect_complete(&mut s, 4);
            drop(s); // server closes the connection while idle
            let mut s = accept(l);
            expect_request(&mut s, stage::REQUEST, 2);
            send_small_file(&mut s, stage::REQUEST, 2, b"two!");
            expect_complete(&mut s, 4);
        });
        assert_eq!(client.download(1, |_, _| {}).unwrap(), b"one!");
        assert_eq!(client.download(2, |_, _| {}).unwrap(), b"two!");
        server.join().unwrap();
    }

    #[test]
    fn unexpected_packet_errors() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, stage::REQUEST, 7);
            s.write_all(&packet(stage::HELLO, 9, &[])).unwrap();
        });
        let err = client.download_raw(7, |_, _| {}).unwrap_err();
        assert!(matches!(
            err,
            FileConnError::UnexpectedPacket {
                stage: stage::HELLO,
                action: 9,
                ..
            }
        ));
        server.join().unwrap();
    }

    #[test]
    fn mismatched_file_id_errors() {
        let (mut client, server) = serve(|l| {
            let mut s = accept(l);
            expect_request(&mut s, stage::REQUEST, 7);
            send_small_file(&mut s, stage::REQUEST, 8, b"xxxx");
        });
        assert!(matches!(
            client.download_raw(7, |_, _| {}),
            Err(FileConnError::Protocol(_))
        ));
        server.join().unwrap();
    }
}
