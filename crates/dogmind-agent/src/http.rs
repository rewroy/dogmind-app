//! HTTP/1.1 over a socket, with the system's TLS for `https`, read so that a cancel can end a request at any moment. A
//! request that ends early closes its connection, and that is what stops a server generating.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(100);
/// Nothing from the server for this long ends the request. A long prompt's prefill sends nothing for minutes.
const IDLE_LIMIT: Duration = Duration::from_secs(1800);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, String> {
        let (tls, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
            (Some(rest), _) => (true, rest),
            (None, Some(rest)) => (false, rest),
            _ => return Err(format!("{url}: a URL starts with http:// or https://")),
        };
        let (authority, path) = rest.split_once('/').map_or((rest, ""), |(authority, path)| (authority, path));
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| format!("{url}: bad port"))?),
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(format!("{url}: no host"));
        }
        Ok(Url { tls, host: host.to_string(), port, path: format!("/{}", path.trim_end_matches('/')) })
    }

    pub fn join(&self, path: &str) -> Url {
        Url { path: format!("{}/{}", self.path.trim_end_matches('/'), path.trim_start_matches('/')), ..self.clone() }
    }
}

#[derive(Debug)]
pub enum Failure {
    Cancelled,
    Connect(String),
    Stream(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Cancelled => write!(f, "cancelled"),
            Failure::Connect(message) | Failure::Stream(message) => write!(f, "{message}"),
        }
    }
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(stream) => stream.read(into),
            Transport::Tls(stream) => stream.read(into),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(stream) => stream.write(bytes),
            Transport::Tls(stream) => stream.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Transport::Plain(stream) => stream.flush(),
            Transport::Tls(stream) => stream.flush(),
        }
    }
}

enum Framing {
    Chunked { left: usize, ended: bool },
    Length(usize),
    Close,
}

pub struct Response<'a> {
    pub status: u16,
    /// By name in lower case.
    headers: Vec<(String, String)>,
    stream: Transport,
    buffer: Vec<u8>,
    framing: Framing,
    cancel: &'a AtomicBool,
}

/// `headers` carry what a provider authenticates with; they go only to `url`.
pub fn request<'a>(method: &str, url: &Url, headers: &[(String, String)], body: Option<&str>, connect: Duration, cancel: &'a AtomicBool) -> Result<Response<'a>, Failure> {
    let address = (url.host.as_str(), url.port).to_socket_addrs().map_err(|error| Failure::Connect(format!("{}: {error}", url.host)))?
        .next().ok_or_else(|| Failure::Connect(format!("{}: no address", url.host)))?;
    request_at(address, method, url, headers, body, connect, cancel)
}

/// To this address, whatever the name would resolve to by now: for a caller that has looked at where it is going.
pub fn request_at<'a>(address: SocketAddr, method: &str, url: &Url, headers: &[(String, String)], body: Option<&str>, connect: Duration, cancel: &'a AtomicBool) -> Result<Response<'a>, Failure> {
    let socket = TcpStream::connect_timeout(&address, connect).map_err(|error| Failure::Connect(format!("{address}: {error}")))?;
    let _ = socket.set_nodelay(true);
    let mut stream = if url.tls {
        // The handshake runs on a blocking socket; once it is done, reads poll as for a plain one.
        socket.set_read_timeout(Some(connect)).map_err(|error| Failure::Connect(error.to_string()))?;
        let connector = native_tls::TlsConnector::new().map_err(|error| Failure::Connect(error.to_string()))?;
        let secured = connector.connect(&url.host, socket).map_err(|error| Failure::Connect(format!("{}: {error}", url.host)))?;
        secured.get_ref().set_read_timeout(Some(POLL)).map_err(|error| Failure::Connect(error.to_string()))?;
        Transport::Tls(Box::new(secured))
    } else {
        socket.set_read_timeout(Some(POLL)).map_err(|error| Failure::Connect(error.to_string()))?;
        Transport::Plain(socket)
    };
    // Keep-alive, and the connection closes when the response has been read: `Connection: close` meets resets on Windows.
    let host = if url.port == if url.tls { 443 } else { 80 } { url.host.clone() } else { format!("{}:{}", url.host, url.port) };
    let accept = if headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("accept")) { "" } else { "Accept: */*\r\n" };
    let mut head = format!("{method} {} HTTP/1.1\r\nHost: {host}\r\n{accept}Connection: keep-alive\r\n", url.path);
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body.unwrap_or("").as_bytes()))
        .map_err(|error| Failure::Stream(error.to_string()))?;
    let mut response = Response { status: 0, headers: Vec::new(), stream, buffer: Vec::new(), framing: Framing::Close, cancel };
    response.read_head()?;
    Ok(response)
}

impl Response<'_> {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(known, _)| known == name).map(|(_, value)| value.as_str())
    }

    /// More bytes off the socket into the buffer; `false` when the server has closed.
    fn fill(&mut self) -> Result<bool, Failure> {
        let started = Instant::now();
        let mut block = [0u8; 16384];
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Failure::Cancelled);
            }
            match self.stream.read(&mut block) {
                Ok(0) => return Ok(false),
                Ok(count) => {
                    self.buffer.extend_from_slice(&block[..count]);
                    return Ok(true);
                }
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    if started.elapsed() > IDLE_LIMIT {
                        return Err(Failure::Stream("the server sent nothing for 30 minutes".into()));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(Failure::Stream(error.to_string())),
            }
        }
    }

    fn read_head(&mut self) -> Result<(), Failure> {
        let end = loop {
            if let Some(at) = self.buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break at;
            }
            if !self.fill()? {
                return Err(Failure::Stream("the server closed the connection before answering".into()));
            }
        };
        let head = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
        self.buffer.drain(..end + 4);
        let mut lines = head.split("\r\n");
        self.status = lines.next().and_then(|line| line.split_whitespace().nth(1)).and_then(|code| code.parse().ok())
            .ok_or_else(|| Failure::Stream(format!("not an HTTP answer: {head}")))?;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else { continue };
            let (name, value) = (name.to_ascii_lowercase(), value.trim());
            self.headers.push((name.clone(), value.to_string()));
            if name == "content-length" {
                self.framing = Framing::Length(value.parse().map_err(|_| Failure::Stream(format!("bad {line}")))?);
            } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                self.framing = Framing::Chunked { left: 0, ended: false };
            }
        }
        Ok(())
    }

    /// The next bytes of the body as they arrive, `None` at its end.
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, Failure> {
        match self.framing {
            Framing::Length(0) => Ok(None),
            Framing::Length(left) => {
                if self.buffer.is_empty() && !self.fill()? {
                    return Err(Failure::Stream("the connection closed inside the answer".into()));
                }
                let take = left.min(self.buffer.len());
                self.framing = Framing::Length(left - take);
                Ok(Some(self.buffer.drain(..take).collect()))
            }
            Framing::Close => {
                if self.buffer.is_empty() && !self.fill()? {
                    return Ok(None);
                }
                Ok(Some(std::mem::take(&mut self.buffer)))
            }
            Framing::Chunked { ended: true, .. } => Ok(None),
            Framing::Chunked { left: 0, .. } => {
                let line = loop {
                    if let Some(at) = self.buffer.windows(2).position(|window| window == b"\r\n") {
                        break at;
                    }
                    if !self.fill()? {
                        return Err(Failure::Stream("the connection closed inside the answer".into()));
                    }
                };
                let text = String::from_utf8_lossy(&self.buffer[..line]).into_owned();
                self.buffer.drain(..line + 2);
                if text.is_empty() {
                    return self.next(); // the line that ends the previous chunk
                }
                let size = usize::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16)
                    .map_err(|_| Failure::Stream(format!("bad chunk size {text}")))?;
                self.framing = Framing::Chunked { left: size, ended: size == 0 };
                self.next()
            }
            Framing::Chunked { left, .. } => {
                if self.buffer.is_empty() && !self.fill()? {
                    return Err(Failure::Stream("the connection closed inside the answer".into()));
                }
                let take = left.min(self.buffer.len());
                self.framing = Framing::Chunked { left: left - take, ended: false };
                Ok(Some(self.buffer.drain(..take).collect()))
            }
        }
    }

    pub fn text(&mut self) -> Result<String, Failure> {
        let mut body = Vec::new();
        while let Some(bytes) = self.next()? {
            body.extend_from_slice(&bytes);
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn serve(answer: &'static [u8]) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            stream.write_all(answer).unwrap();
            // The client closes first, as with a keep-alive server: a server closing first can reset what is unread.
            while matches!(stream.read(&mut request), Ok(count) if count > 0) {}
        });
        Url { tls: false, host: "127.0.0.1".into(), port, path: "/".into() }
    }

    #[test]
    fn urls_join_under_their_path() {
        let url = Url::parse("http://127.0.0.1:8080/v1").unwrap();
        assert_eq!(url.join("chat/completions").path, "/v1/chat/completions");
        assert_eq!(Url::parse("http://localhost:8080").unwrap().join("/health").path, "/health");
        let secure = Url::parse("https://api.anthropic.com/v1").unwrap();
        assert!(secure.tls && secure.port == 443);
        assert_eq!(secure.join("messages").path, "/v1/messages");
        assert!(Url::parse("ftp://example.com").is_err());
        assert!(Url::parse("http://").is_err());
    }

    #[test]
    fn a_chunked_body_reads_whole_across_its_chunks() {
        let cancel = AtomicBool::new(false);
        let url = serve(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n");
        let mut response = request("GET", &url, &[], None, Duration::from_secs(5), &cancel).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.text().unwrap(), "hello world");
    }

    #[test]
    fn a_body_ends_at_its_length() {
        let cancel = AtomicBool::new(false);
        let url = serve(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 2\r\n\r\nok");
        let mut response = request("GET", &url, &[], None, Duration::from_secs(5), &cancel).unwrap();
        assert_eq!((response.status, response.text().unwrap().as_str()), (503, "ok"));
    }
}
