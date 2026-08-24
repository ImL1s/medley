//! Blocking HTTP/1 header drain for probe fixtures.
//!
//! A one-shot `read` on an accepted stream is not a complete request-header
//! contract: the listener may be nonblocking (macOS inherits that onto the
//! accepted `TcpStream`), and TCP may deliver a partial request. Callers must
//! return the stream to blocking mode, bound the read, and loop until
//! `\r\n\r\n` (#317).

use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// Cap on request-header bytes so a missing terminator cannot grow forever.
pub const DEFAULT_MAX_HTTP_HEADER_BYTES: usize = 8192;

/// Accept the next connection, polling `WouldBlock` until `deadline`.
pub fn accept_with_deadline(listener: &TcpListener, deadline: Instant) -> io::Result<TcpStream> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "listener did not accept a connection before the deadline",
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Read until the HTTP header terminator, handling partial reads.
///
/// Forces blocking mode and a read timeout so a missing request fails quickly
/// instead of hanging or treating `WouldBlock` as fatal.
pub fn read_http_request_headers(
    stream: &mut TcpStream,
    read_timeout: Duration,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(read_timeout))?;
    let mut request = Vec::with_capacity(2048);
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        if request.len() >= max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("HTTP request headers exceeded {max_bytes} bytes"),
            ));
        }
        let mut chunk = [0u8; 1024];
        let read_len = (max_bytes - request.len()).min(chunk.len());
        match stream.read(&mut chunk[..read_len]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed before complete request headers",
                ));
            }
            Ok(read) => request.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn read_http_request_headers_assembles_partial_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream.write_all(b"GET /v1/api-key HTTP/1.1\r\n").unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            stream.write_all(b"Host: 127.0.0.1\r\n\r\n").unwrap();
            stream.flush().unwrap();
        });

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request_headers(
            &mut stream,
            Duration::from_secs(2),
            DEFAULT_MAX_HTTP_HEADER_BYTES,
        )
        .expect("complete headers");
        let text = String::from_utf8_lossy(&request);
        assert!(
            text.starts_with("GET /v1/api-key "),
            "assembled request must keep the production probe path: {text}"
        );
        assert!(
            text.contains("\r\n\r\n"),
            "drain must wait for the header terminator: {text:?}"
        );
        client.join().unwrap();
    }

    #[test]
    fn read_http_request_headers_times_out_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = std::net::TcpStream::connect(addr).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let started = Instant::now();
        let error = read_http_request_headers(
            &mut stream,
            Duration::from_millis(50),
            DEFAULT_MAX_HTTP_HEADER_BYTES,
        )
        .expect_err("missing request must not hang");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "bounded read timeout must fail quickly, took {:?}",
            started.elapsed()
        );
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::UnexpectedEof
            ),
            "expected a bounded I/O failure, got {error:?}"
        );
    }

    #[test]
    fn accept_then_drain_survives_nonblocking_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            // Delay the request so a one-shot nonblocking read would miss it.
            std::thread::sleep(Duration::from_millis(30));
            stream
                .write_all(b"GET /v1/api-key HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .unwrap();
        });

        let mut stream = accept_with_deadline(&listener, Instant::now() + Duration::from_secs(2))
            .expect("accept probe connection");
        let request = read_http_request_headers(
            &mut stream,
            Duration::from_secs(1),
            DEFAULT_MAX_HTTP_HEADER_BYTES,
        )
        .expect("drain delayed probe request");
        assert!(
            String::from_utf8_lossy(&request).starts_with("GET /v1/api-key "),
            "delayed request must still be a complete GET /v1/api-key"
        );
        client.join().unwrap();
    }
}
