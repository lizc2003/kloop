//! A raw HTTP fixture for shapes wiremock cannot express: a response body
//! flushed in separate chunks. wiremock hands the client one byte string, so a
//! rail that stops reading at its terminal frame still sees the whole tail and
//! the bug hides.

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

/// How long each chunk is held back from the next. Long enough that the client
/// reads the first chunk on its own, short enough not to slow the suite.
const CHUNK_GAP: Duration = Duration::from_millis(50);

pub(crate) struct ChunkedSse {
    base: String,
}

impl ChunkedSse {
    /// Serve exactly one request, writing `chunks` as separate flushes.
    pub(crate) fn start(chunks: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind chunked sse fixture");
        let address = listener.local_addr().expect("chunked sse address");
        // Detached: the thread ends with the response, and a test that never
        // connects must not block its own teardown.
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream.set_nodelay(true).ok();
            if read_request(&mut stream) {
                write_chunks(&mut stream, &chunks).ok();
            }
        });
        Self {
            base: format!("http://{address}"),
        }
    }

    pub(crate) fn base(&self) -> &str {
        &self.base
    }
}

/// Consume the request so closing the socket does not reset the connection
/// under the response. `false` means the client vanished before sending it.
fn read_request(stream: &mut TcpStream) -> bool {
    let mut received = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return false,
            Ok(read) => read,
        };
        received.extend_from_slice(&buffer[..read]);
        let Some(header_end) = received.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&received[..header_end]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if received.len() >= header_end + 4 + length {
            return true;
        }
    }
}

fn write_chunks(stream: &mut TcpStream, chunks: &[String]) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          content-type: text/event-stream\r\n\
          connection: close\r\n\
          transfer-encoding: chunked\r\n\r\n",
    )?;
    stream.flush()?;
    for chunk in chunks {
        write!(stream, "{:x}\r\n", chunk.len())?;
        stream.write_all(chunk.as_bytes())?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        thread::sleep(CHUNK_GAP);
    }
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}
