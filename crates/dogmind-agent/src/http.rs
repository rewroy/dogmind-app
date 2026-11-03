//! HTTP/1.1 over a socket, with the system's TLS for `https`, read so that a cancel can end a request at any moment. A
//! request that ends early closes its connection, and that is what stops a server generating: Straydog hears nothing else,
//! and a client that only stops reading holds up its next request for 30 s and more.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
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

impl Transport {
    /// Ends the connection itself, for whoever holds the socket: a drop only lets go of this handle, and with another still open the server hears nothing.
    fn shut(&self) {
        let socket = match self {
            Transport::Plain(stream) => stream,
            Transport::Tls(stream) => stream.get_ref(),
        };
        let _ = socket.shutdown(Shutdown::Both);
    }

    /// All of these bytes, with an ear for a stop: a server that has stopped reading would otherwise hold up the person's Stop for as long as it likes. Without a timeout on the
    /// socket, as Windows calls a socket whose send timed out indeterminate. A TLS stream cannot be left in the middle of a record, so that one blocks.
    fn send(&mut self, mut bytes: &[u8], cancel: &AtomicBool) -> Result<(), Failure> {
        let failed = |error: io::Error| Failure::Stream(error.to_string());
        let Transport::Plain(stream) = self else {
            // (what blocks can still be ended from beside it, which ends the connection: that is what a stop means here)
            let socket = match self {
                Transport::Tls(stream) => stream.get_ref().try_clone().map_err(failed)?,
                Transport::Plain(_) => unreachable!("just looked at"),
            };
            let sent = with_an_ear_for_a_stop(&socket, cancel, || self.write_all(bytes));
            return sent.map_err(|error| if cancel.load(Ordering::Relaxed) { Failure::Cancelled } else { failed(error) });
        };
        stream.set_nonblocking(true).map_err(failed)?;
        let started = Instant::now();
        while !bytes.is_empty() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Failure::Cancelled);
            }
            match stream.write(bytes) {
                Ok(0) => return Err(Failure::Stream("the connection closed".into())),
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if started.elapsed() > IDLE_LIMIT {
                        return Err(Failure::Stream("the server took nothing for 30 minutes".into()));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(failed(error)),
            }
        }
        stream.set_nonblocking(false).map_err(failed)
    }
}

/// Runs a blocking write, and if a stop comes while it blocks, shuts the socket under it: a server that has stopped reading would otherwise hold up the person's Stop for as long as it likes.
fn with_an_ear_for_a_stop<T>(socket: &TcpStream, cancel: &AtomicBool, write: impl FnOnce() -> T) -> T {
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    let _ = socket.shutdown(Shutdown::Both);
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let result = write();
        done.store(true, Ordering::Relaxed);
        result
    })
}

/// What a head, or a chunk's size line, comes to at most: past this it is no web server, and memory is all it could cost.
const MOST_OF_A_LINE: usize = 1 << 20;

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
    let addresses: Vec<SocketAddr> = (url.host.as_str(), url.port).to_socket_addrs().map_err(|error| Failure::Connect(format!("{}: {error}", url.host)))?.collect();
    request_at(&addresses, method, url, headers, body, connect, cancel)
}

/// How long an address with others after it is given. Many machines have an IPv6 address and no IPv6 route, and a name's IPv6 addresses come first:
/// they would take the whole of `connect`, for each of them, before the one that answers.
const BEFORE_THE_NEXT_ADDRESS: Duration = Duration::from_millis(1500);

/// One of each family first, then the rest in the order given.
fn in_order(addresses: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut order: Vec<SocketAddr> = Vec::with_capacity(addresses.len());
    if let Some(first) = addresses.first() {
        order.push(*first);
        order.extend(addresses.iter().find(|address| address.is_ipv4() != first.is_ipv4()));
    }
    for address in addresses {
        if !order.contains(address) {
            order.push(*address);
        }
    }
    order
}

/// The first of these that takes the connection, trying the other family second.
fn connect_to(addresses: &[SocketAddr], connect: Duration, cancel: &AtomicBool) -> Result<TcpStream, Failure> {
    let order = in_order(addresses);
    let mut failed = None;
    for (at, address) in order.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Failure::Cancelled);
        }
        let allowed = if at + 1 < order.len() { connect.min(BEFORE_THE_NEXT_ADDRESS) } else { connect };
        match TcpStream::connect_timeout(address, allowed) {
            Ok(socket) => return Ok(socket),
            Err(error) => failed = Some(format!("{address}: {error}")),
        }
    }
    Err(Failure::Connect(failed.unwrap_or_else(|| "no address".to_string())))
}

/// Whether anything takes a connection at this address.
pub fn answers(url: &Url) -> bool {
    let addresses: Vec<SocketAddr> = (url.host.as_str(), url.port).to_socket_addrs().map(Iterator::collect).unwrap_or_default();
    connect_to(&addresses, Duration::from_secs(5), &AtomicBool::new(false)).is_ok()
}

/// To these addresses, whatever the name would resolve to by now: for a caller that has looked at where it is going.
pub fn request_at<'a>(addresses: &[SocketAddr], method: &str, url: &Url, headers: &[(String, String)], body: Option<&str>, connect: Duration, cancel: &'a AtomicBool) -> Result<Response<'a>, Failure> {
    let socket = connect_to(addresses, connect, cancel)?;
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
    let sent = stream.send(head.as_bytes(), cancel).and_then(|()| stream.send(body.unwrap_or("").as_bytes(), cancel));
    if let Err(failure) = sent {
        stream.shut();
        return Err(failure);
    }
    let mut response = Response { status: 0, headers: Vec::new(), stream, buffer: Vec::new(), framing: Framing::Close, cancel };
    response.read_head()?;
    Ok(response)
}

/// However a response ends: read to its end, cancelled, or failed.
impl Drop for Response<'_> {
    fn drop(&mut self) {
        self.stream.shut();
    }
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
            if self.buffer.len() > MOST_OF_A_LINE {
                return Err(Failure::Stream("the server's answer has no end to its head".into()));
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
        // These have no body, whatever they say: with no length and the connection kept open, reading to its close would wait out the half hour.
        if matches!(self.status, 100..=199 | 204 | 304) {
            self.framing = Framing::Length(0);
        }
        Ok(())
    }

    /// The next bytes of the body as they arrive, `None` at its end.
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, Failure> {
        // (a loop, and no call of itself: a stranger's page of nothing but empty lines would otherwise end the app with the stack it took)
        loop {
            if let Some(bytes) = self.next_step()? {
                return Ok(bytes);
            }
        }
    }

    /// `None`: a chunk's line has been read, and there is still nothing to give.
    fn next_step(&mut self) -> Result<Option<Option<Vec<u8>>>, Failure> {
        match self.framing {
            Framing::Length(0) => Ok(Some(None)),
            Framing::Length(left) => {
                if self.buffer.is_empty() && !self.fill()? {
                    return Err(Failure::Stream("the connection closed inside the answer".into()));
                }
                let take = left.min(self.buffer.len());
                self.framing = Framing::Length(left - take);
                Ok(Some(Some(self.buffer.drain(..take).collect())))
            }
            Framing::Close => {
                if self.buffer.is_empty() && !self.fill()? {
                    return Ok(Some(None));
                }
                Ok(Some(Some(std::mem::take(&mut self.buffer))))
            }
            Framing::Chunked { ended: true, .. } => Ok(Some(None)),
            Framing::Chunked { left: 0, .. } => {
                let line = loop {
                    if let Some(at) = self.buffer.windows(2).position(|window| window == b"\r\n") {
                        break at;
                    }
                    if self.buffer.len() > MOST_OF_A_LINE {
                        return Err(Failure::Stream("a chunk's size has no end to its line".into()));
                    }
                    if !self.fill()? {
                        return Err(Failure::Stream("the connection closed inside the answer".into()));
                    }
                };
                let text = String::from_utf8_lossy(&self.buffer[..line]).into_owned();
                self.buffer.drain(..line + 2);
                if text.is_empty() {
                    return Ok(None); // the line that ends the previous chunk
                }
                let size = usize::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16)
                    .map_err(|_| Failure::Stream(format!("bad chunk size {text}")))?;
                self.framing = Framing::Chunked { left: size, ended: size == 0 };
                Ok(None)
            }
            Framing::Chunked { left, .. } => {
                if self.buffer.is_empty() && !self.fill()? {
                    return Err(Failure::Stream("the connection closed inside the answer".into()));
                }
                let take = left.min(self.buffer.len());
                self.framing = Framing::Chunked { left: left - take, ended: false };
                Ok(Some(Some(self.buffer.drain(..take).collect())))
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
pub(crate) mod tests {
    /// A name's first address often answers nothing at all (an IPv6 address on a machine with no IPv6 route): the reader must not stop there.
    #[test]
    fn an_address_that_does_not_answer_does_not_end_it() {
        let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = {
            let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            taken.local_addr().unwrap()
        };
        let good = open.local_addr().unwrap();
        let cancel = AtomicBool::new(false);
        let socket = connect_to(&[closed, good], Duration::from_secs(5), &cancel).unwrap();
        assert_eq!(socket.peer_addr().unwrap(), good);
        let failed = connect_to(&[closed], Duration::from_secs(5), &cancel).err().unwrap();
        assert!(matches!(failed, Failure::Connect(_)));
    }

    #[test]
    fn the_other_family_is_tried_second_and_a_stop_is_heard() {
        let (six, four): (SocketAddr, SocketAddr) = ("[::1]:9".parse().unwrap(), "127.0.0.1:9".parse().unwrap());
        assert_eq!(in_order(&[six, six, four]), [six, four]);
        let cancel = AtomicBool::new(true);
        assert!(matches!(connect_to(&[four], Duration::from_secs(5), &cancel), Err(Failure::Cancelled)));
    }

    use super::*;
    use std::net::TcpListener;

    /// A listener for a scripted server, at the first free port from 18550.
    pub(crate) fn scripted_listener() -> TcpListener {
        (18550..19000).find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok()).expect("a port from 18550")
    }

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
    fn an_answer_with_no_body_has_none_and_is_not_waited_for() {
        for answer in [&b"HTTP/1.1 204 No Content\r\n\r\n"[..], &b"HTTP/1.1 304 Not Modified\r\nETag: \"x\"\r\n\r\n"[..]] {
            let url = serve(answer);
            let cancel = AtomicBool::new(false);
            let (tell, told) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut response = request("GET", &url, &[], None, Duration::from_secs(5), &cancel).unwrap();
                let _ = tell.send(response.text());
            });
            assert_eq!(told.recv_timeout(Duration::from_secs(10)).expect("not the half hour it would wait for a close").unwrap(), "");
        }
    }

    /// What has arrived already, read as a response, over a socket of its own with nothing more coming: no server is in the way of what is being looked at.
    fn arrived<'a>(bytes: &[u8], framing: Framing, cancel: &'a AtomicBool) -> (Response<'a>, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        stream.set_read_timeout(Some(POLL)).unwrap();
        let (held, _) = listener.accept().unwrap();
        (Response { status: 0, headers: Vec::new(), stream: Transport::Plain(stream), buffer: bytes.to_vec(), framing, cancel }, held)
    }

    #[test]
    fn a_page_of_empty_lines_does_not_end_the_app() {
        let mut answer: Vec<u8> = std::iter::repeat_n(&b"\r\n"[..], 40_000).flatten().copied().collect();
        answer.extend_from_slice(b"2\r\nok\r\n0\r\n\r\n");
        let cancel = AtomicBool::new(false);
        // (on the smallest stack a request is ever read on)
        std::thread::scope(|scope| {
            std::thread::Builder::new().stack_size(1 << 18).spawn_scoped(scope, || {
                let (mut response, _held) = arrived(&answer, Framing::Chunked { left: 0, ended: false }, &cancel);
                assert_eq!(response.text().unwrap(), "ok");
            }).unwrap();
        });
    }

    #[test]
    fn a_head_or_a_chunks_line_with_no_end_is_not_read_for_ever() {
        let cancel = AtomicBool::new(false);
        let mut no_head = b"HTTP/1.1 200 OK\r\n".to_vec();
        no_head.extend(std::iter::repeat_n(b'x', 2 << 20));
        let (mut response, _held) = arrived(&no_head, Framing::Close, &cancel);
        assert!(response.read_head().err().unwrap().to_string().contains("no end"));
        let (mut response, _held) = arrived(&vec![b'1'; 2 << 20], Framing::Chunked { left: 0, ended: false }, &cancel);
        assert!(response.text().err().unwrap().to_string().contains("no end"));
    }

    /// As TLS is written, blocking, to a server that has stopped reading.
    #[test]
    fn a_stop_ends_a_write_that_blocks() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_held, _) = listener.accept().unwrap();
        let cancel = AtomicBool::new(false);
        let socket = stream.try_clone().unwrap();
        let block = vec![0u8; 1 << 20];
        let began = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Relaxed);
            });
            let written = with_an_ear_for_a_stop(&socket, &cancel, || loop {
                if let Err(error) = stream.write_all(&block) {
                    break error;
                }
            });
            assert!(!written.to_string().is_empty());
        });
        assert!(began.elapsed() < Duration::from_secs(20), "{:?}", began.elapsed());
    }

    /// A local server that goes on writing an answer, as an engine does, until it hears that its client has gone. The address, and when it heard.
    fn endless_answer() -> (Url, std::sync::mpsc::Receiver<Instant>) {
        let listener = scripted_listener();
        let port = listener.local_addr().unwrap().port();
        let (heard, hearing) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut writer, _) = listener.accept().unwrap();
            let mut reader = writer.try_clone().unwrap();
            let mut request = [0u8; 4096];
            let _ = reader.read(&mut request);
            let told = heard.clone();
            std::thread::spawn(move || {
                let mut block = [0u8; 256];
                while matches!(reader.read(&mut block), Ok(count) if count > 0) {}
                let _ = told.send(Instant::now());
            });
            writer.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            while writer.write_all(b"a\r\n0123456789\r\n").is_ok() {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = heard.send(Instant::now());
        });
        (Url { tls: false, host: "127.0.0.1".into(), port, path: "/".into() }, hearing)
    }

    fn reads_until_cancelled(response: &mut Response, cancel: &AtomicBool) {
        assert!(response.next().ok().unwrap().is_some());
        cancel.store(true, Ordering::Relaxed);
        while !matches!(response.next(), Err(Failure::Cancelled)) {}
    }

    #[test]
    fn a_cancel_closes_the_connection_at_once() {
        let (url, hearing) = endless_answer();
        let cancel = AtomicBool::new(false);
        let mut response = request("POST", &url, &[], Some("{}"), Duration::from_secs(5), &cancel).ok().unwrap();
        reads_until_cancelled(&mut response, &cancel);
        let closed = Instant::now();
        drop(response);
        let heard = hearing.recv_timeout(Duration::from_secs(5)).expect("the server hears of it");
        let took = heard.saturating_duration_since(closed);
        eprintln!("the server heard of the close after {} ms", took.as_millis());
        assert!(took < Duration::from_millis(500), "{took:?}");
    }

    /// As a duplicate of the socket would: with only a close, the server goes on writing until the connection's buffers are full, and an engine then waits 30 s before it gives up.
    #[test]
    fn a_cancel_closes_the_connection_whoever_else_holds_the_socket() {
        let (url, hearing) = endless_answer();
        let cancel = AtomicBool::new(false);
        let mut response = request("POST", &url, &[], Some("{}"), Duration::from_secs(5), &cancel).ok().unwrap();
        let Transport::Plain(socket) = &response.stream else { panic!("plain") };
        let other = socket.try_clone().unwrap();
        reads_until_cancelled(&mut response, &cancel);
        let closed = Instant::now();
        drop(response);
        let heard = hearing.recv_timeout(Duration::from_secs(5)).expect("the server hears of it");
        assert!(heard.saturating_duration_since(closed) < Duration::from_millis(500));
        drop(other);
    }

    /// A stop before an answer begins: a long prefill sends nothing for minutes.
    #[test]
    fn a_cancel_is_heard_before_the_answer_begins() {
        let listener = scripted_listener();
        let port = listener.local_addr().unwrap().port();
        let (heard, hearing) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut block = [0u8; 4096];
            while matches!(stream.read(&mut block), Ok(count) if count > 0) {}
            let _ = heard.send(());
        });
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let stopper = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stopper.store(true, Ordering::Relaxed);
        });
        let url = Url { tls: false, host: "127.0.0.1".into(), port, path: "/".into() };
        let started = Instant::now();
        assert!(matches!(request("POST", &url, &[], Some("{}"), Duration::from_secs(5), &cancel), Err(Failure::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));
        hearing.recv_timeout(Duration::from_secs(2)).expect("the server hears of it");
    }

    /// A server that reads nothing, and a body larger than what the system holds for it: the Stop is heard all the same.
    #[test]
    fn a_cancel_is_heard_while_a_body_is_going_out() {
        let listener = scripted_listener();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let accepted = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(10));
            drop(accepted);
        });
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let stopper = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            stopper.store(true, Ordering::Relaxed);
        });
        let url = Url { tls: false, host: "127.0.0.1".into(), port, path: "/".into() };
        let body = "x".repeat(16 << 20);
        let started = Instant::now();
        assert!(matches!(request("POST", &url, &[], Some(&body), Duration::from_secs(5), &cancel), Err(Failure::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
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
