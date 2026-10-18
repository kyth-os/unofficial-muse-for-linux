//! HTTP transports: TLS-only production client plus a loopback test client.
//!
//! [`ReqwestTransport`] (production) refuses any non-`https` URL, uses the
//! OS trust store via rustls, and applies bounded connect/total timeouts.
//! [`LoopbackTransport`] speaks plain HTTP **only to 127.0.0.1/::1** over a
//! real TCP socket; it exists so tests can run against a scripted fake
//! CalDAV server with genuine socket I/O and no internet. It is not used
//! by production code paths.

use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Bounded timeouts shared by both transports.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// A minimal HTTP response. Bodies are capped by the caller.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }
}

#[derive(Debug, Clone)]
pub enum TransportError {
    /// Used by PUT when the request was sent but no usable response arrived.
    Uncertain(String),
    Failed(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uncertain(m) => write!(f, "uncertain outcome: {m}"),
            Self::Failed(m) => write!(f, "transport failure: {m}"),
        }
    }
}

/// Injectable HTTP layer. The client adds auth; transports move bytes.
pub trait HttpTransport: Send + Sync {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, TransportError>;
}

// ---------------------------------------------------------------------------
// Production: reqwest blocking + rustls, https only.
// ---------------------------------------------------------------------------

/// Production transport. Rejects plaintext URLs unconditionally.
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

/// Cap applied while streaming a response body: per-call caps in the client
/// are smaller, but no single response may buffer more than this.
pub const TRANSPORT_MAX_BYTES: u64 = 8 * 1024 * 1024;

impl ReqwestTransport {
    pub fn new() -> Result<Self, TransportError> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(CONNECT_TIMEOUT + IO_TIMEOUT)
            // Never follow redirects: the https check below covers only the
            // initial URL, and a 302 to http:// (or another host) would
            // otherwise carry the Basic credential with it. Redirects
            // surface as ordinary 3xx statuses for the caller to reject.
            .redirect(reqwest::redirect::Policy::none())
            // No plain-HTTP fallback is possible: every request URL is
            // https:// (enforced below) and reqwest has no downgrade path.
            .build()
            .map_err(|e| TransportError::Failed(format!("http client setup: {e}")))?;
        Ok(Self { client })
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new().expect("reqwest client builds")
    }
}

impl HttpTransport for ReqwestTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, TransportError> {
        if !url.starts_with("https://") {
            return Err(TransportError::Failed(
                "refusing non-TLS URL: CalDAV requires https".into(),
            ));
        }
        let mut req = self.client.request(
            method
                .parse()
                .map_err(|_| TransportError::Failed("bad HTTP method".into()))?,
            url,
        );
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req.body(b.to_vec());
        }
        let resp = req.send().map_err(|e| {
            if e.is_timeout() || e.is_body() || e.is_decode() {
                TransportError::Uncertain(format!("no usable response: {e}"))
            } else {
                TransportError::Failed(format!("request failed: {e}"))
            }
        })?;
        let status = resp.status().as_u16();
        let mut hdrs = HashMap::new();
        for (k, v) in resp.headers() {
            if let Ok(v) = v.to_str() {
                hdrs.insert(k.as_str().to_lowercase(), v.to_string());
            }
        }
        // Bound memory before buffering: a declared length above the cap is
        // rejected without allocating, and the body is streamed through a
        // take() so an unbounded stream cannot OOM the process either.
        if resp
            .content_length()
            .is_some_and(|n| n > TRANSPORT_MAX_BYTES)
        {
            return Err(TransportError::Failed(
                "response declares more than the transport cap".into(),
            ));
        }
        let mut bytes = Vec::new();
        resp.take(TRANSPORT_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| TransportError::Uncertain(format!("response body unreadable: {e}")))?;
        if bytes.len() as u64 > TRANSPORT_MAX_BYTES {
            return Err(TransportError::Failed(
                "response exceeds the transport cap".into(),
            ));
        }
        Ok(HttpResponse {
            status,
            headers: hdrs,
            body: bytes,
        })
    }
}

// ---------------------------------------------------------------------------
// Test: raw-socket loopback HTTP client (127.0.0.1 / ::1 only).
// ---------------------------------------------------------------------------

/// Test-only transport doing real socket I/O against a loopback fake server.
pub struct LoopbackTransport;

impl LoopbackTransport {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LoopbackTransport {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_test_url(url: &str) -> Result<(String, u16, String), TransportError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| TransportError::Failed("loopback transport needs http:// URLs".into()))?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| TransportError::Failed("bad test URL port".into()))?,
        ),
        None => (host_port.to_string(), 80),
    };
    if host != "127.0.0.1" && host != "::1" && host != "localhost" {
        return Err(TransportError::Failed(
            "loopback transport only talks to 127.0.0.1/::1".into(),
        ));
    }
    Ok((host, port, path))
}

impl HttpTransport for LoopbackTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, TransportError> {
        let (host, port, path) = parse_test_url(url)?;
        let addrs = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| TransportError::Failed(format!("resolve loopback: {e}")))?;
        let addr = addrs
            .into_iter()
            .next()
            .ok_or_else(|| TransportError::Failed("no loopback address".into()))?;
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| TransportError::Failed(format!("connect loopback: {e}")))?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|e| TransportError::Failed(format!("socket setup: {e}")))?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|e| TransportError::Failed(format!("socket setup: {e}")))?;

        let body_len = body.map(|b| b.len()).unwrap_or(0);
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nhost: {host}:{port}\r\nconnection: close\r\ncontent-length: {body_len}\r\n"
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        // A write failure *before* any byte is usefully sent is Failed; once
        // the request is on the wire, ambiguity belongs to the caller. The
        // raw-socket client cannot tell, so PUT callers treat every error
        // after this point as Uncertain (see client.rs).
        stream
            .write_all(head.as_bytes())
            .map_err(|e| TransportError::Failed(format!("write head: {e}")))?;
        if let Some(b) = body {
            stream
                .write_all(b)
                .map_err(|e| TransportError::Failed(format!("write body: {e}")))?;
        }

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| TransportError::Uncertain(format!("no response: {e}")))?;
        if status_line.trim().is_empty() {
            return Err(TransportError::Uncertain(
                "server closed connection without a response".into(),
            ));
        }
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                TransportError::Uncertain(format!(
                    "unparseable status line '{}'",
                    status_line.trim_end()
                ))
            })?;
        let mut hdrs = HashMap::new();
        let mut content_len: Option<usize> = None;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|e| TransportError::Uncertain(format!("response head unreadable: {e}")))?;
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_lowercase();
                let v = v.trim().to_string();
                if k == "content-length" {
                    content_len = v.parse().ok();
                }
                hdrs.insert(k, v);
            }
        }
        // Cap single-read bodies at the transport cap; per-call caps are
        // enforced later. A declared length above the cap is rejected
        // without allocating; an undeclared stream goes through take() so
        // it cannot OOM the process before the check.
        let cap = TRANSPORT_MAX_BYTES as usize;
        let mut body = Vec::new();
        match content_len {
            Some(n) => {
                if n > cap {
                    return Err(TransportError::Failed(
                        "response declares more than the transport cap".into(),
                    ));
                }
                body.resize(n, 0);
                reader.read_exact(&mut body).map_err(|e| {
                    TransportError::Uncertain(format!("response body truncated: {e}"))
                })?;
            }
            None => {
                reader
                    .take(cap as u64 + 1)
                    .read_to_end(&mut body)
                    .map_err(|e| {
                        TransportError::Uncertain(format!("response body unreadable: {e}"))
                    })?;
                if body.len() > cap {
                    return Err(TransportError::Failed(
                        "response exceeds the transport cap".into(),
                    ));
                }
            }
        }
        Ok(HttpResponse {
            status,
            headers: hdrs,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prod_transport_rejects_plaintext() {
        let t = ReqwestTransport::new().unwrap();
        let err = t
            .request("GET", "http://127.0.0.1:9/x", &[], None)
            .unwrap_err();
        assert!(matches!(err, TransportError::Failed(_)));
    }

    #[test]
    fn loopback_transport_rejects_non_loopback() {
        let t = LoopbackTransport::new();
        let err = t
            .request("GET", "http://example.com/x", &[], None)
            .unwrap_err();
        assert!(matches!(err, TransportError::Failed(_)));
    }

    /// Serve one raw HTTP response, then run the loopback client against it.
    fn raw_exchange(head: Vec<u8>, body: Vec<u8>) -> Result<HttpResponse, TransportError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Drain the client request head (GET has no body here).
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read head");
                if line.trim_end().is_empty() {
                    break;
                }
            }
            stream.write_all(&head).expect("write head");
            // The client is expected to disconnect once it has seen more
            // than the transport cap; a reset here is the behavior under
            // test, not a server failure.
            let _ = stream.write_all(&body);
        });
        let out = LoopbackTransport::new().request(
            "GET",
            &format!("http://127.0.0.1:{}/x", addr.port()),
            &[],
            None,
        );
        server.join().expect("server thread");
        out
    }

    #[test]
    fn loopback_rejects_declared_huge_body_before_allocating() {
        // Regression: the old code sized the buffer from the declared
        // Content-Length. A 100 MB declaration must fail without reading.
        let err = raw_exchange(
            b"HTTP/1.1 200 OK\r\ncontent-length: 100000000\r\nconnection: close\r\n\r\n".to_vec(),
            b"short".to_vec(),
        )
        .unwrap_err();
        assert!(
            matches!(err, TransportError::Failed(_)),
            "declared-huge must fail closed, got {err:?}"
        );
    }

    #[test]
    fn loopback_caps_undeclared_streams() {
        // Regression: read_to_end + truncate let an unbounded stream OOM
        // the process. 9 MiB with no Content-Length must fail closed.
        let big = vec![b'z'; 9 * 1024 * 1024];
        let err = raw_exchange(
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_vec(),
            big,
        )
        .unwrap_err();
        assert!(
            matches!(err, TransportError::Failed(_)),
            "over-cap stream must fail closed, got {err:?}"
        );
    }
}
