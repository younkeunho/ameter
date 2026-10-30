//! A minimal Chrome DevTools protocol client: one local WebSocket (RFC 6455), text messages,
//! one request at a time. Events between requests are skipped.
//!
//! A browser whose remote debugging was turned on in chrome://inspect asks the user before it
//! accepts a client, and holds the WebSocket handshake until then. [`Handshake`] therefore
//! waits in steps without giving up the connection, so one connection means one question.

use crate::util::JsonExt;
use serde_json::{Map, Value, json};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;
/// DevTools replies here are small; anything larger is a protocol error.
const MAX_MESSAGE: usize = 16 << 20;
const MAX_HEAD: usize = 8192;

#[derive(Debug)]
pub enum CdpError {
    /// The connection is unusable.
    Io(io::Error),
    /// The browser answered with an error, or the page did; the connection is fine.
    Protocol(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Io(e) => write!(f, "{e}"),
            CdpError::Protocol(message) => f.write_str(message),
        }
    }
}

fn fail(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// An upgrade request sent to `ws://127.0.0.1:{port}{path}`, waiting for the browser's answer.
pub struct Handshake {
    stream: TcpStream,
    head: Vec<u8>,
}

impl Handshake {
    pub fn start(port: u16, path: &str) -> io::Result<Handshake> {
        let mut stream = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(2))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let key = base64(&random_bytes::<16>());
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        Ok(Handshake { stream, head: Vec::new() })
    }

    /// Waits up to `wait` for the answer: true once the browser accepted, false while it is still
    /// asking. An error means it refused or closed the connection.
    pub fn progress(&mut self, wait: Duration) -> io::Result<bool> {
        // Non-blocking reads: a timed-out blocking read leaves a Windows socket unusable.
        self.stream.set_nonblocking(true)?;
        let deadline = Instant::now() + wait;
        let mut byte = [0u8; 1];
        // Byte by byte, so nothing after the header is consumed.
        while !self.head.ends_with(b"\r\n\r\n") {
            match self.stream.read(&mut byte) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the browser closed the connection")),
                Ok(_) => {
                    self.head.push(byte[0]);
                    if self.head.len() > MAX_HEAD {
                        return Err(fail("handshake response too long"));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e),
            }
        }
        self.stream.set_nonblocking(false)?;
        let head = String::from_utf8_lossy(&self.head);
        let status = head.lines().next().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("101") {
            return Err(fail(format!("handshake refused: {status}")));
        }
        Ok(true)
    }

    /// The accepted connection; every later read and write gives up after `timeout`.
    pub fn into_cdp(self, timeout: Duration) -> io::Result<Cdp> {
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.set_write_timeout(Some(timeout))?;
        Ok(Cdp { stream: self.stream, next_id: 0 })
    }
}

pub struct Cdp {
    stream: TcpStream,
    next_id: i64,
}

impl Cdp {
    /// Sends a command and returns its `result`.
    pub fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Map<String, Value>, CdpError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            request["sessionId"] = session.into();
        }
        self.write_frame(OP_TEXT, request.to_string().as_bytes()).map_err(CdpError::Io)?;
        loop {
            let text = self.read_message().map_err(CdpError::Io)?;
            let Ok(Value::Object(reply)) = serde_json::from_str::<Value>(&text) else { continue };
            if reply.i64_of("id") != Some(id) {
                continue;
            }
            if let Some(error) = reply.obj("error") {
                return Err(CdpError::Protocol(format!("{method}: {}", error.str_of("message").unwrap_or("error"))));
            }
            return Ok(reply.obj("result").cloned().unwrap_or_default());
        }
    }

    /// Evaluates `expression` in a page session, awaiting a promise, and returns its value.
    pub fn evaluate(&mut self, session: &str, expression: &str) -> Result<Value, CdpError> {
        let params = json!({ "expression": expression, "awaitPromise": true, "returnByValue": true });
        let result = self.call("Runtime.evaluate", params, Some(session))?;
        if result.obj("exceptionDetails").is_some() {
            return Err(CdpError::Protocol("Runtime.evaluate: the script threw".into()));
        }
        Ok(result.obj("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null))
    }

    /// One masked, unfragmented client frame.
    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n <= 0xFFFF => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = random_bytes::<4>();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame)
    }

    /// The next text message, reassembled from fragments. Pings are answered on the way.
    fn read_message(&mut self) -> io::Result<String> {
        let mut message = Vec::new();
        loop {
            let mut head = [0u8; 2];
            self.stream.read_exact(&mut head)?;
            let fin = head[0] & 0x80 != 0;
            let opcode = head[0] & 0x0F;
            let masked = head[1] & 0x80 != 0;
            let len = match head[1] & 0x7F {
                126 => {
                    let mut b = [0u8; 2];
                    self.stream.read_exact(&mut b)?;
                    u16::from_be_bytes(b) as usize
                }
                127 => {
                    let mut b = [0u8; 8];
                    self.stream.read_exact(&mut b)?;
                    usize::try_from(u64::from_be_bytes(b)).unwrap_or(usize::MAX)
                }
                n => n as usize,
            };
            if len > MAX_MESSAGE || message.len() + len > MAX_MESSAGE {
                return Err(fail("message too large"));
            }
            let mut mask = [0u8; 4];
            if masked {
                self.stream.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len];
            self.stream.read_exact(&mut payload)?;
            if masked {
                payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
            }
            match opcode {
                OP_CLOSE => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the browser closed the connection")),
                OP_PING => self.write_frame(OP_PONG, &payload)?,
                OP_TEXT | OP_CONTINUATION => {
                    message.extend_from_slice(&payload);
                    if fin {
                        return String::from_utf8(message).map_err(|_| fail("message is not UTF-8"));
                    }
                }
                _ => {}
            }
        }
    }
}

/// OS-seeded random bytes from the standard library's hasher keys.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    for chunk in out.chunks_mut(8) {
        let value = std::collections::hash_map::RandomState::new().build_hasher().finish();
        chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"the sample nonce"), "dGhlIHNhbXBsZSBub25jZQ==");
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        String::from_utf8(request).unwrap()
    }

    /// Server side of one frame: unmasked, with the given first byte.
    fn server_frame(first: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![first];
        if payload.len() < 126 {
            frame.push(payload.len() as u8);
        } else {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        frame
    }

    /// Reads one masked client frame and returns (opcode, payload).
    fn client_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut head = [0u8; 2];
        stream.read_exact(&mut head).unwrap();
        assert_ne!(head[1] & 0x80, 0, "client frames must be masked");
        let len = match head[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                stream.read_exact(&mut b).unwrap();
                u16::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask).unwrap();
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).unwrap();
        payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
        (head[0] & 0x0F, payload)
    }

    const ACCEPT: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";

    #[test]
    fn the_handshake_keeps_waiting_while_the_browser_asks_the_user() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (answer, answered) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("GET /devtools/browser/x HTTP/1.1\r\n"));
            assert!(request.contains("Sec-WebSocket-Key: "));
            answered.recv().unwrap(); // the user clicks "Allow"
            stream.write_all(ACCEPT).unwrap();
            let (opcode, payload) = client_frame(&mut stream);
            assert_eq!(opcode, OP_TEXT);
            let call: Value = serde_json::from_slice(&payload).unwrap();
            stream.write_all(&server_frame(0x81, format!(r#"{{"id":{},"result":{{}}}}"#, call["id"]).as_bytes())).unwrap();
        });

        let mut handshake = Handshake::start(port, "/devtools/browser/x").unwrap();
        assert!(!handshake.progress(Duration::from_millis(150)).unwrap());
        assert!(!handshake.progress(Duration::from_millis(150)).unwrap());
        answer.send(()).unwrap();
        assert!(handshake.progress(Duration::from_secs(5)).unwrap());
        let mut cdp = handshake.into_cdp(Duration::from_secs(5)).unwrap();
        assert!(cdp.call("Target.getTargets", json!({}), None).unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn a_refused_or_closed_handshake_is_an_error() {
        for answer in [b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".as_slice(), b"".as_slice()] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_request(&mut stream);
                stream.write_all(answer).unwrap();
            });
            let mut handshake = Handshake::start(port, "/devtools/browser/x").unwrap();
            assert!(handshake.progress(Duration::from_secs(5)).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn a_call_skips_events_answers_pings_and_joins_fragments() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            stream.write_all(ACCEPT).unwrap();

            let (opcode, payload) = client_frame(&mut stream);
            assert_eq!(opcode, OP_TEXT);
            let call: Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(call["method"], "Target.getTargets");
            assert_eq!(call["sessionId"], "s1");

            stream.write_all(&server_frame(0x89, b"hi")).unwrap();
            let (opcode, payload) = client_frame(&mut stream);
            assert_eq!((opcode, payload.as_slice()), (OP_PONG, b"hi".as_slice()));

            stream.write_all(&server_frame(0x81, br#"{"method":"Target.targetCreated","params":{}}"#)).unwrap();
            stream.write_all(&server_frame(0x81, br#"{"id":99,"result":{}}"#)).unwrap();
            let reply = format!(r#"{{"id":1,"result":{{"note":"{}"}}}}"#, "x".repeat(300));
            let (a, b) = reply.as_bytes().split_at(10);
            stream.write_all(&server_frame(0x01, a)).unwrap();
            stream.write_all(&server_frame(0x80, b)).unwrap();

            let (opcode, payload) = client_frame(&mut stream);
            let call: Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!((opcode, call["id"].as_i64()), (OP_TEXT, Some(2)));
            stream.write_all(&server_frame(0x81, br#"{"id":2,"error":{"message":"No target"}}"#)).unwrap();

            client_frame(&mut stream);
            stream.write_all(&server_frame(0x88, b"")).unwrap();
        });

        let mut handshake = Handshake::start(port, "/devtools/browser/x").unwrap();
        assert!(handshake.progress(Duration::from_secs(5)).unwrap());
        let mut cdp = handshake.into_cdp(Duration::from_secs(5)).unwrap();
        let result = cdp.call("Target.getTargets", json!({}), Some("s1")).unwrap();
        assert_eq!(result.str_of("note").map(str::len), Some(300));
        match cdp.call("Target.closeTarget", json!({}), None) {
            Err(CdpError::Protocol(message)) => assert_eq!(message, "Target.closeTarget: No target"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
        assert!(matches!(cdp.call("Target.getTargets", json!({}), None), Err(CdpError::Io(_))));
        server.join().unwrap();
    }
}
