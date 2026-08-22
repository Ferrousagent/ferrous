//! End-to-end tests for the `ferrous wterm` server: it must serve the
//! terminal page over HTTP and stream a real `ferrous shell` session over a
//! WebSocket PTY.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may unwrap freely

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// The RFC 6455 sample key: any valid base64 is accepted by the server.
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const MASK: [u8; 4] = [0x11, 0x22, 0x33, 0x44];

/// Kills the server process when dropped, even on test panic.
struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `ferrous wterm --port 0` and return the bound port from stdout.
fn spawn_server() -> (ServerGuard, u16) {
    let binary = assert_cmd::cargo::cargo_bin("ferrous");
    let mut child = Command::new(binary)
        .args(["wterm", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawns ferrous wterm");
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader
            .read_line(&mut line)
            .map(|count| count > 0)
            .unwrap_or(false)
        {
            if let Some(port) = parse_port(&line) {
                let _ = sender.send(port);
                return;
            }
            line.clear();
        }
    });
    let port = receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("wterm printed its address");
    (ServerGuard(child), port)
}

/// Extract the bound port from the printed listen line.
fn parse_port(line: &str) -> Option<u16> {
    let marker = "127.0.0.1:";
    let start = line.find(marker)? + marker.len();
    let digits: String = line[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Fetch a path over plain HTTP and return the raw response bytes.
fn http_get_bytes(port: u16, path: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .expect("writes request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("reads response");
    response
}

/// Fetch a path and decode the response as lossy text.
fn http_get(port: u16, path: &str) -> String {
    String::from_utf8_lossy(&http_get_bytes(port, path)).into_owned()
}

/// Perform a WebSocket client handshake and return the upgraded stream.
fn ws_connect(port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    write!(
        stream,
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .expect("writes handshake");
    let mut handshake = Vec::new();
    let mut byte = [0u8; 1];
    while !handshake.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("reads handshake");
        handshake.push(byte[0]);
    }
    let response = String::from_utf8_lossy(&handshake);
    assert!(
        response.starts_with("HTTP/1.1 101"),
        "expected 101 upgrade, got: {response:?}"
    );
    stream
}

/// Send a masked binary frame (client-to-server frames must be masked).
fn ws_send_binary(stream: &mut TcpStream, payload: &[u8]) {
    assert!(
        payload.len() < 126,
        "test payloads stay in the 7-bit length"
    );
    let header = [0x82, 0x80 | payload.len() as u8];
    stream.write_all(&header).expect("writes frame header");
    stream.write_all(&MASK).expect("writes mask");
    let masked: Vec<u8> = payload
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ MASK[index % 4])
        .collect();
    stream.write_all(&masked).expect("writes payload");
    stream.flush().expect("flushes");
}

/// Read one server frame; server-to-client frames are unmasked.
fn ws_read_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).ok()?;
    let opcode = header[0] & 0x0f;
    let length = (header[1] & 0x7f) as usize;
    let length = if length == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended).ok()?;
        u16::from_be_bytes(extended) as usize
    } else {
        length
    };
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).ok()?;
    Some((opcode, payload))
}

/// Emulate the one piece of terminal behaviour the PTY depends on: ConPTY on
/// Windows asks the terminal for its cursor position (`ESC[6n`) and holds the
/// child's output/input until it answers. A real terminal emulator (wterm)
/// replies automatically; the test client must too, or Windows never streams
/// the banner or echoes keystrokes.
fn answer_terminal_queries(stream: &mut TcpStream, payload: &[u8]) {
    if payload.windows(4).any(|window| window == b"\x1b[6n") {
        ws_send_binary(stream, b"\x1b[1;1R");
    }
}

/// Read frames until all `needles` appear in the accumulated text.
fn read_until(stream: &mut TcpStream, needles: &[&str]) -> String {
    let mut collected = String::new();
    loop {
        match ws_read_frame(stream) {
            Some((0x8, _)) => break, // close frame
            Some((_, payload)) => {
                answer_terminal_queries(stream, &payload);
                collected.push_str(&String::from_utf8_lossy(&payload));
                if needles.iter().all(|needle| collected.contains(needle)) {
                    break;
                }
            }
            None => break, // read timed out or connection closed
        }
    }
    collected
}

/// Read frames until a close frame or EOF, returning everything received.
fn read_all(stream: &mut TcpStream) -> String {
    let mut collected = String::new();
    loop {
        match ws_read_frame(stream) {
            Some((0x8, _)) => break,
            Some((_, payload)) => {
                answer_terminal_queries(stream, &payload);
                collected.push_str(&String::from_utf8_lossy(&payload));
            }
            None => break,
        }
    }
    collected
}

#[test]
fn wterm_serves_the_terminal_page_and_assets() {
    let (server, port) = spawn_server();

    let page = http_get(port, "/");
    assert!(page.starts_with("HTTP/1.1 200"));
    assert!(page.contains("ferrous · wterm"), "page title missing");
    assert!(page.contains("@wterm/react"), "wterm import missing");

    let wasm = http_get_bytes(port, "/wterm.wasm");
    assert!(wasm.starts_with(b"HTTP/1.1 200"));
    assert!(
        wasm.windows(4).any(|window| window == b"\0asm"),
        "wasm magic"
    );

    let css = http_get(port, "/wterm.css");
    assert!(css.starts_with("HTTP/1.1 200"));
    assert!(css.contains("wterm"));

    let missing = http_get(port, "/nope");
    assert!(missing.starts_with("HTTP/1.1 404"));

    drop(server);
}

#[test]
fn wterm_ws_streams_a_real_shell_session() {
    let (server, port) = spawn_server();
    let mut socket = ws_connect(port);

    // The shell echoes input through the pty and answers `pwd` with the
    // workspace path; the banner proves the child session started.
    ws_send_binary(&mut socket, b"pwd\r\n");
    let output = read_until(&mut socket, &["pwd", "ferrous shell"]);
    assert!(
        output.contains("ferrous shell"),
        "shell banner missing from: {output:?}"
    );
    assert!(output.contains("pwd"), "typed input not echoed: {output:?}");

    // `exit` closes the child; the server should send a close frame or EOF.
    ws_send_binary(&mut socket, b"exit\r\n");
    let final_output = read_all(&mut socket);
    assert!(
        final_output.contains("bye") || final_output.is_empty(),
        "expected clean exit output, got: {final_output:?}"
    );

    drop(server);
}
