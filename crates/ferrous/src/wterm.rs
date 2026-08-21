//! The `ferrous wterm` server: the Ferrous shell inside a browser terminal.
//!
//! A tiny, thread-per-connection HTTP + WebSocket server. Each WebSocket
//! connection spawns the real `ferrous shell` as a PTY child (via
//! `portable-pty`) and streams bytes between the browser terminal
//! ([`@wterm/react`](https://wterm.dev)) and the shell, so line editing,
//! echo, history, and the interactive approval prompt behave like a real
//! terminal. No async runtime is needed: the socket is polled with a short
//! read timeout and the PTY is drained with non-blocking `try_read`.
//!
//! Static assets (the terminal page, the wterm core WASM binary, and the
//! wterm stylesheet) are embedded at compile time and served over HTTP.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use anyhow::Context;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tungstenite::{Message, WebSocket, accept};

/// The single-file terminal page (React + `@wterm/react` via esm.sh).
const INDEX_HTML: &str = include_str!("../web/index.html");
/// The wterm core WASM binary (VT escape-sequence parser, ~18 KB).
const WTERM_WASM: &[u8] = include_bytes!("../web/wterm.wasm");
/// The wterm terminal stylesheet (themes + renderer layout).
const WTERM_CSS: &str = include_str!("../web/terminal.css");

/// How long to wait for a slow client during the HTTP request phase.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Socket read timeout used while polling the terminal connection.
const POLL_READ_TIMEOUT: Duration = Duration::from_millis(20);
/// Upper bound for a single HTTP request head (line + headers).
const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// Options for the `ferrous wterm` server.
pub struct WtermOptions {
    /// Interface to bind (default `127.0.0.1`).
    pub host: String,
    /// TCP port to bind; `0` selects an ephemeral port.
    pub port: u16,
}

/// Run the wterm server until the process is terminated.
///
/// # Errors
///
/// Returns an error if the listener cannot be bound.
pub fn run(options: WtermOptions) -> anyhow::Result<()> {
    let listener = TcpListener::bind((options.host.as_str(), options.port))
        .with_context(|| format!("failed to bind {}:{}", options.host, options.port))?;
    let address = listener
        .local_addr()
        .context("failed to read the bound address")?;
    println!("ferrous wterm — open http://{address} in a browser");
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let _ = std::thread::spawn(move || {
            let _ = handle_connection(stream);
        });
    }
    Ok(())
}

/// Serve a single connection: static assets over HTTP, the terminal over WS.
fn handle_connection(mut stream: TcpStream) -> anyhow::Result<()> {
    let request = read_request(&mut stream)?;
    let request_text = String::from_utf8_lossy(&request).into_owned();
    if route_of(&request_text) == Route::Terminal {
        return serve_terminal(stream, request);
    }
    write_http_response(stream, route_of(&request_text)).map_err(anyhow::Error::from)
}

/// The routes this server knows how to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// The terminal page.
    Index,
    /// The wterm core WASM binary.
    Wasm,
    /// The wterm stylesheet.
    Css,
    /// The WebSocket terminal endpoint.
    Terminal,
    /// Anything else.
    NotFound,
}

/// Map an HTTP request head (line + headers) to a route.
fn route_of(request: &str) -> Route {
    let first_line = request.lines().next().unwrap_or_default();
    let path = first_line.split_whitespace().nth(1).unwrap_or_default();
    match path {
        "/" | "/index.html" => Route::Index,
        "/wterm.wasm" => Route::Wasm,
        "/wterm.css" => Route::Css,
        "/ws" => Route::Terminal,
        _ => Route::NotFound,
    }
}

/// Read the complete HTTP request head (request line + headers) so the bytes
/// can be replayed verbatim to the WebSocket handshake. Reads one byte at a
/// time so nothing beyond the blank line is consumed from the socket.
fn read_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(HTTP_READ_TIMEOUT))?;
    let mut request = Vec::new();
    let mut matched = 0usize;
    let mut byte = [0u8; 1];
    loop {
        if request.len() > MAX_REQUEST_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head exceeds the size limit",
            ));
        }
        match stream.read(&mut byte)? {
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed before the request head",
                ));
            }
            _ => {
                request.push(byte[0]);
                if byte[0] == b"\r\n\r\n"[matched] {
                    matched += 1;
                    if matched == 4 {
                        return Ok(request);
                    }
                } else {
                    matched = 0;
                    if byte[0] == b'\r' {
                        matched = 1;
                    }
                }
            }
        }
    }
}

/// A `Read + Write` wrapper that yields buffered bytes before the socket, so
/// a consumed HTTP request can be replayed to the WebSocket handshake.
struct Prepend {
    prefix: Vec<u8>,
    position: usize,
    inner: TcpStream,
}

impl Read for Prepend {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position < self.prefix.len() {
            let count = (self.prefix.len() - self.position).min(buffer.len());
            buffer[..count].copy_from_slice(&self.prefix[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        } else {
            self.inner.read(buffer)
        }
    }
}

impl Write for Prepend {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Write a minimal HTTP response for a static route.
fn write_http_response(mut stream: TcpStream, route: Route) -> io::Result<()> {
    let (status, content_type, body) = match route {
        Route::Index => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        Route::Wasm => ("200 OK", "application/wasm", WTERM_WASM),
        Route::Css => ("200 OK", "text/css; charset=utf-8", WTERM_CSS.as_bytes()),
        Route::Terminal => (
            "426 Upgrade Required",
            "text/plain; charset=utf-8",
            b"websocket upgrade required".as_slice(),
        ),
        Route::NotFound => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found".as_slice(),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         \r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

/// Upgrade a connection to a WebSocket terminal backed by a PTY shell.
fn serve_terminal(stream: TcpStream, request: Vec<u8>) -> anyhow::Result<()> {
    stream
        .set_read_timeout(Some(POLL_READ_TIMEOUT))
        .context("failed to set the socket poll timeout")?;
    let prepend = Prepend {
        prefix: request,
        position: 0,
        inner: stream,
    };
    let mut socket = accept(prepend).context("websocket handshake failed")?;

    let program = std::env::current_exe().context("failed to locate the ferrous binary")?;
    let pty_system = native_pty_system();
    let mut pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open a pty")?;
    let mut command = CommandBuilder::new(program);
    command.arg("shell");
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("failed to spawn ferrous shell")?;
    drop(pair.slave);

    let result = run_terminal_loop(&mut socket, pair.master.as_mut(), child.as_mut());
    let _ = socket.send(Message::Close(None));
    let _ = child.kill();
    result
}
/// Bridge the WebSocket and the PTY until one side closes.
///
/// The socket is polled with a short read timeout so pty output is relayed
/// promptly, and a dedicated thread performs the blocking pty read so the
/// loop can keep serving the socket while the child is quiet. Writes to the
/// pty are blocking, giving natural backpressure.
fn run_terminal_loop<S: Read + Write>(
    socket: &mut WebSocket<S>,
    master: &mut dyn MasterPty,
    child: &mut dyn Child,
) -> anyhow::Result<()> {
    let mut reader = master
        .try_clone_reader()
        .context("failed to clone the pty reader")?;
    let mut writer = master
        .take_writer()
        .context("failed to take the pty writer")?;
    let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if sender.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    loop {
        match socket.read() {
            Ok(Message::Binary(bytes)) => {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
            Ok(Message::Text(text)) => handle_control(master, &text),
            Ok(Message::Ping(payload)) => {
                let _ = socket.send(Message::Pong(payload));
            }
            Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
            Ok(Message::Close(_)) => break,
            Err(tungstenite::Error::Io(ref error))
                if error.kind() == io::ErrorKind::TimedOut
                    || error.kind() == io::ErrorKind::WouldBlock =>
            {
                // Nothing arrived within the poll window: fall through to
                // relay pty output and check the child.
            }
            Err(_) => break,
        }

        while let Ok(bytes) = receiver.try_recv() {
            if socket.send(Message::Binary(bytes.into())).is_err() {
                return Ok(());
            }
        }

        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Handle a client control message. Only `{"resize":[cols,rows]}` is defined.
fn handle_control(master: &mut dyn MasterPty, text: &str) {
    let Some(open) = text.find('[') else {
        return;
    };
    let Some(close) = text[open + 1..].find(']') else {
        return;
    };
    let numbers: Vec<u32> = text[open + 1..open + 1 + close]
        .split(',')
        .filter_map(|part| part.trim().parse::<u32>().ok())
        .collect();
    if numbers.len() == 2 {
        let _ = master.resize(PtySize {
            rows: numbers[1].clamp(2, 500) as u16,
            cols: numbers[0].clamp(2, 500) as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn route_of_maps_known_paths() {
        assert_eq!(route_of("GET / HTTP/1.1"), Route::Index);
        assert_eq!(route_of("GET /index.html HTTP/1.1"), Route::Index);
        assert_eq!(route_of("GET /wterm.wasm HTTP/1.1"), Route::Wasm);
        assert_eq!(route_of("GET /wterm.css HTTP/1.1"), Route::Css);
        assert_eq!(route_of("GET /ws HTTP/1.1"), Route::Terminal);
        assert_eq!(route_of("GET /favicon.ico HTTP/1.1"), Route::NotFound);
        assert_eq!(route_of("bogus"), Route::NotFound);
    }

    #[test]
    fn read_request_captures_the_full_head() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let address = listener.local_addr().expect("address");
        let mut client = std::net::TcpStream::connect(address).expect("connects");
        let (mut server, _) = listener.accept().expect("accepts");
        client
            .write_all(b"GET /wterm.wasm HTTP/1.1\r\nHost: x\r\n\r\nbody")
            .expect("writes");
        let request = read_request(&mut server).expect("reads");
        assert_eq!(request, b"GET /wterm.wasm HTTP/1.1\r\nHost: x\r\n\r\n");
        // The body after the head stays unread on the socket.
        let mut rest = [0u8; 4];
        server.read_exact(&mut rest).expect("reads the rest");
        assert_eq!(&rest, b"body");
    }

    #[test]
    fn route_of_accepts_full_requests() {
        assert_eq!(
            route_of("GET /ws HTTP/1.1\r\nHost: x\r\n\r\n"),
            Route::Terminal
        );
        assert_eq!(route_of("GET / HTTP/1.1\r\n\r\n"), Route::Index);
    }

    #[test]
    fn control_resize_never_panics() {
        // A resize payload without a pty is a no-op that must not panic, and
        // malformed payloads are ignored.
        handle_control(&mut *stub_master(), "{\"resize\":[120,40]}");
        handle_control(&mut *stub_master(), "{\"resize\":[9999]}");
        handle_control(&mut *stub_master(), "not json");
        handle_control(&mut *stub_master(), "");
    }

    /// A real (unused) pty so control handling can be unit-tested.
    fn stub_master() -> Box<dyn MasterPty> {
        let pty_system = native_pty_system();
        pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("opens a pty")
            .master
    }
}
