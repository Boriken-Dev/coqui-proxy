#![cfg(feature = "relay-blocking")]

//! The relay without Tokio: `CONNECT` and absolute-form `http:` requests sent through the
//! connector's routes (failover, login, direct), its credential, and its bounds. The cases mirror
//! the asynchronous relay's.

mod support;

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use coqui_proxy::relay::blocking::{Relay, RelayHandle};
use coqui_proxy::{Credentials, Environment, Error, ProxyConnector, RelaySettings, Resolver};
use support::socks_server::SocksServer;
use support::{Require, StandInOrigin, StandInProxy};

fn basic(handle: &RelayHandle) -> String {
    let credentials = handle.credentials();
    let pair = format!("{}:{}", credentials.username(), credentials.password());
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

fn resolver(variable: &str, value: String) -> Resolver {
    Resolver::builder()
        .environment(Environment::from_pairs([(variable, value)]))
        .credentials(Credentials::new("ada", "s3cret"))
        .build()
}

/// A resolver that sends everything direct.
fn direct() -> Resolver {
    Resolver::builder()
        .environment(Environment::from_pairs([("X", "y")]))
        .system(false)
        .build()
}

fn relay(resolver: Resolver) -> RelayHandle {
    Relay::builder(ProxyConnector::default(), resolver)
        .start()
        .unwrap()
}

fn small(change: impl FnOnce(&mut RelaySettings)) -> RelayHandle {
    let mut settings = RelaySettings::default();
    change(&mut settings);
    Relay::builder(ProxyConnector::default(), direct())
        .settings(settings)
        .start()
        .unwrap()
}

fn dead_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A listener that accepts and drops every connection, counting them, on a port that was dead
/// before: a connector's attempt on it is seen.
struct Flaky {
    attempts: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    address: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl Flaky {
    fn on(port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (counted, stopped) = (Arc::clone(&attempts), Arc::clone(&stop));
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                counted.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        Self {
            attempts,
            stop,
            address,
            thread: Some(thread),
        }
    }
}

impl Drop for Flaky {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn connect(handle: &RelayHandle) -> TcpStream {
    let stream = TcpStream::connect(handle.address()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
}

/// Reads one reply: its head, and a body of `Content-Length` bytes.
fn read_reply(stream: &mut TcpStream) -> (String, String) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let length = content_length(&head);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).unwrap();
    (head, String::from_utf8(body).unwrap())
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0)
}

fn request(handle: &RelayHandle, text: &str) -> (String, String) {
    let mut stream = connect(handle);
    stream.write_all(text.as_bytes()).unwrap();
    read_reply(&mut stream)
}

fn get_through(handle: &RelayHandle, url: &str, extra: &str) -> String {
    let host = url.split('/').nth(2).unwrap();
    format!(
        "GET {url} HTTP/1.1\r\nHost: {host}\r\nProxy-Authorization: {}\r\n{extra}Connection: close\r\n\r\n",
        basic(handle)
    )
}

/// Sends `CONNECT`, checks the answer and returns the tunnel.
fn tunnel(handle: &RelayHandle, target: &str) -> TcpStream {
    let mut stream = connect(handle);
    stream
        .write_all(
            format!(
                "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: {}\r\n\r\n",
                basic(handle)
            )
            .as_bytes(),
        )
        .unwrap();
    let (head, _) = read_reply(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    stream
}

fn get_in(stream: &mut TcpStream, host: &str) -> String {
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    reply
}

#[test]
fn both_kinds_of_request_fail_over_from_a_dead_proxy_and_skip_it_afterwards() {
    let origin = StandInOrigin::start("via the second");
    let live = StandInProxy::start(Require::Nothing, origin.addr);
    let dead = dead_port();
    let handle = relay(resolver(
        "ALL_PROXY",
        format!("PROXY 127.0.0.1:{dead}; PROXY {}", live.addr),
    ));

    let mut connected = tunnel(&handle, "app.internal.test:8443");
    assert!(get_in(&mut connected, "app.internal.test").ends_with("via the second"));

    // Something now listens where the dead proxy was: a second attempt on it would be counted.
    let listening = Flaky::on(dead);
    let (head, body) = request(
        &handle,
        &get_through(&handle, "http://app.internal.test/data?x=1", ""),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "via the second");
    // The failed proxy is tried after the live one, which answers, so it is not asked again.
    assert_eq!(listening.attempts.load(Ordering::SeqCst), 0);
    let connects: Vec<_> = live
        .seen()
        .into_iter()
        .map(|seen| seen.request_line)
        .collect();
    assert_eq!(
        connects,
        [
            "CONNECT app.internal.test:8443 HTTP/1.1",
            "CONNECT app.internal.test:80 HTTP/1.1"
        ]
    );

    // Control: a relay that has not seen it fail tries it first.
    let fresh = relay(resolver(
        "ALL_PROXY",
        format!("PROXY 127.0.0.1:{dead}; PROXY {}", live.addr),
    ));
    let (head, _) = request(
        &fresh,
        &get_through(&fresh, "http://app.internal.test/", ""),
    );
    // On Linux a listener that drops the connection reads as a refusal, which ends failover: 502.
    assert!(
        head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.1 502"),
        "{head}"
    );
    assert!(listening.attempts.load(Ordering::SeqCst) >= 1);
}

#[test]
fn requests_go_through_a_proxy_that_wants_a_login() {
    let origin = StandInOrigin::start("behind a login");
    let live = StandInProxy::start(Require::Basic("ada", "s3cret"), origin.addr);
    let handle = relay(resolver("ALL_PROXY", live.proxy_url()));

    let mut connected = tunnel(&handle, "app.internal.test:8443");
    assert!(get_in(&mut connected, "app.internal.test").ends_with("behind a login"));
    let (head, body) = request(
        &handle,
        &get_through(&handle, "http://app.internal.test/", ""),
    );
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "behind a login");
    for seen in live.seen() {
        assert_eq!(
            seen.header("proxy-authorization"),
            Some("Basic YWRhOnMzY3JldA==")
        );
    }
}

#[test]
fn requests_go_through_a_socks_proxy_and_direct() {
    let origin = StandInOrigin::start("the origin");
    let socks = SocksServer::start(origin.addr, None);
    let handle = relay(resolver("ALL_PROXY", format!("socks5://{}", socks.addr)));
    let (_, body) = request(&handle, &get_through(&handle, "http://mail.test/", ""));
    assert_eq!(body, "the origin");
    assert_eq!(socks.asked()[0].destination, "mail.test");

    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/direct", origin.addr.port());
    let (head, body) = request(&handle, &get_through(&handle, &url, ""));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "the origin");
    let mut connected = tunnel(&handle, &format!("127.0.0.1:{}", origin.addr.port()));
    assert!(get_in(&mut connected, "127.0.0.1").ends_with("the origin"));
}

#[test]
fn a_request_is_forwarded_in_origin_form_without_hop_by_hop_headers() {
    let origin = StandInOrigin::start("seen");
    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/path/to?q=1&r=2", origin.addr.port());
    let extra = "X-Kept: yes\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nX-Secret: hidden\r\nTE: trailers\r\n";
    let text = get_through(&handle, &url, extra)
        .replace("Connection: close", "Connection: close, X-Secret");
    let (head, _) = request(&handle, &text);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let seen = origin.seen();
    assert_eq!(seen[0].request_line, "GET /path/to?q=1&r=2 HTTP/1.1");
    assert_eq!(
        seen[0].header("host"),
        Some(format!("127.0.0.1:{}", origin.addr.port()).as_str())
    );
    assert_eq!(seen[0].header("x-kept"), Some("yes"));
    for name in [
        "proxy-authorization",
        "keep-alive",
        "proxy-connection",
        "x-secret",
        "te",
    ] {
        assert_eq!(seen[0].header(name), None, "{name}");
    }
    // The reply's hop-by-hop headers do not reach the client either.
    assert!(!head.to_ascii_lowercase().contains("keep-alive"), "{head}");
}

#[test]
fn the_clients_connection_is_kept_alive_for_the_next_request() {
    let origin = StandInOrigin::start("again");
    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let mut stream = connect(&handle);
    for _ in 0..2 {
        let text = get_through(&handle, &url, "").replace("Connection: close\r\n", "");
        stream.write_all(text.as_bytes()).unwrap();
        let (head, body) = read_reply(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "again");
    }
    assert_eq!(origin.seen().len(), 2);
}

#[test]
fn a_request_without_the_credential_or_with_a_wrong_one_is_refused() {
    let origin = StandInOrigin::start("never");
    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let anonymous = format!("GET {url} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let (head, _) = request(&handle, &anonymous);
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("proxy-authenticate: basic")
    );

    let wrong = base64::engine::general_purpose::STANDARD.encode("coqui-relay:not-the-password");
    let text = format!(
        "GET {url} HTTP/1.1\r\nHost: x\r\nProxy-Authorization: Basic {wrong}\r\nConnection: close\r\n\r\n"
    );
    let (head, _) = request(&handle, &text);
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");

    // A tunnel is refused the same way, and nothing reached the origin.
    let (head, _) = request(
        &handle,
        &format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            origin.addr.port()
        ),
    );
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
    assert!(origin.seen().is_empty());

    let (head, _) = request(&handle, &get_through(&handle, &url, ""));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

#[test]
fn a_destination_no_route_reaches_is_502_and_anything_else_is_405() {
    let handle = relay(resolver(
        "ALL_PROXY",
        format!("socks5://127.0.0.1:{}", dead_port()),
    ));
    let (head, body) = request(&handle, &get_through(&handle, "http://app.test/", ""));
    assert!(head.starts_with("HTTP/1.1 502"), "{head}");
    assert!(body.contains("no reachable proxy"), "{body}");
    let (head, _) = request(
        &handle,
        &format!(
            "CONNECT app.test:443 HTTP/1.1\r\nHost: app.test\r\nProxy-Authorization: {}\r\nConnection: close\r\n\r\n",
            basic(&handle)
        ),
    );
    assert!(head.starts_with("HTTP/1.1 502"), "{head}");

    let origin_form = format!(
        "GET / HTTP/1.1\r\nHost: x\r\nProxy-Authorization: {}\r\nConnection: close\r\n\r\n",
        basic(&handle)
    );
    let (head, _) = request(&handle, &origin_form);
    assert!(head.starts_with("HTTP/1.1 405"), "{head}");
    let (head, _) = request(&handle, &get_through(&handle, "https://app.test/", ""));
    assert!(head.starts_with("HTTP/1.1 405"), "{head}");
    let (head, _) = request(&handle, "this is not http\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
}

#[test]
fn the_credential_is_random_for_each_relay_and_never_printed() {
    let first = relay(direct());
    let second = relay(direct());
    let password = first.credentials().password().to_string();
    assert!(password.len() >= 43, "{password}");
    assert_ne!(password, second.credentials().password());
    for text in [
        format!("{first:?}"),
        format!("{:?}", first.credentials()),
        format!("{:?}", first.proxy()),
        first.proxy_url(),
        format!("{:?}", Relay::builder(ProxyConnector::default(), direct())),
    ] {
        assert!(!text.contains(&password), "{text}");
    }
    assert_eq!(
        first.proxy_url(),
        format!("http://127.0.0.1:{}", first.address().port())
    );
    assert!(first.address().ip().is_loopback());
}

#[test]
fn the_settings_that_are_not_meaningful_or_not_loopback_are_refused() {
    let build = |change: &dyn Fn(&mut RelaySettings)| {
        let mut settings = RelaySettings::default();
        change(&mut settings);
        Relay::builder(ProxyConnector::default(), direct())
            .settings(settings)
            .build()
    };
    assert!(matches!(
        build(&|s| s.address = "0.0.0.0".parse().unwrap()),
        Err(Error::Settings(_))
    ));
    assert!(matches!(
        build(&|s| s.address = "192.0.2.1".parse().unwrap()),
        Err(Error::Settings(_))
    ));
    assert!(build(&|s| s.address = "::1".parse().unwrap()).is_ok());
    assert!(build(&|s| s.max_connections = 0).is_err());
    assert!(build(&|s| s.idle_timeout = Duration::ZERO).is_err());
    assert!(build(&|s| s.header_timeout = Duration::ZERO).is_err());
    assert!(build(&|s| s.accept_retry = Duration::ZERO).is_err());
    assert!(build(&|s| s.poll_interval = Duration::ZERO).is_err());
    assert!(build(&|s| s.max_head_bytes = 100).is_err());
    // Not waiting at all for the threads is a meaningful choice.
    assert!(build(&|s| s.shutdown_wait = Duration::ZERO).is_ok());
    assert!(build(&|_| {}).is_ok());

    let mut loose = RelaySettings::default();
    loose.max_connections *= 2;
    loose.idle_timeout *= 2;
    loose.shutdown_wait *= 2;
    let builder = Relay::builder(ProxyConnector::default(), direct()).settings(loose);
    assert_eq!(builder.non_defaults().len(), 3);
    assert!(
        Relay::builder(ProxyConnector::default(), direct())
            .non_defaults()
            .is_empty()
    );
    // Every setting means something to this relay.
    assert!(builder.not_applied().is_empty());
}

#[test]
fn a_head_longer_than_the_limit_is_refused() {
    let handle = small(|s| s.max_head_bytes = RelaySettings::MIN_MAX_HEAD_BYTES);
    let mut stream = connect(&handle);
    let long = "a".repeat(RelaySettings::MIN_MAX_HEAD_BYTES * 2);
    let text = format!(
        "GET http://x.test/ HTTP/1.1\r\nHost: x.test\r\nX-Long: {long}\r\nProxy-Authorization: {}\r\n\r\n",
        basic(&handle)
    );
    let _ = stream.write_all(text.as_bytes());
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    assert!(
        reply.is_empty() || reply.starts_with("HTTP/1.1 431"),
        "{reply}"
    );
    assert!(!reply.contains("200"), "{reply}");
    // The relay still serves.
    let (head, _) = request(
        &handle,
        "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
}

#[test]
fn a_silent_connection_is_closed_after_the_idle_time() {
    let origin = StandInOrigin::start("again");
    let handle = small(|s| s.idle_timeout = Duration::from_millis(300));
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let mut stream = connect(&handle);
    let text = get_through(&handle, &url, "").replace("Connection: close\r\n", "");
    stream.write_all(text.as_bytes()).unwrap();
    let (head, _) = read_reply(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // Between authenticated requests the wait is the idle time, not the header time (30 s).
    let started = Instant::now();
    let mut buffer = [0u8; 16];
    let read = stream.read(&mut buffer);
    assert!(matches!(read, Ok(0) | Err(_)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_silent_tunnel_is_closed_after_the_idle_time() {
    let origin = StandInOrigin::start("held");
    let handle = small(|s| s.idle_timeout = Duration::from_millis(400));
    let mut stream = tunnel(&handle, &format!("127.0.0.1:{}", origin.addr.port()));
    let started = Instant::now();
    let mut buffer = [0u8; 16];
    assert!(matches!(stream.read(&mut buffer), Ok(0) | Err(_)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_slow_head_is_cut_off_after_the_header_time() {
    let handle = small(|s| {
        s.header_timeout = Duration::from_millis(300);
        s.idle_timeout = Duration::from_secs(60);
    });
    let mut stream = connect(&handle);
    stream
        .write_all(b"GET http://x.test/ HTTP/1.1\r\nHost: x")
        .unwrap();
    let started = Instant::now();
    let mut buffer = [0u8; 256];
    assert!(matches!(stream.read(&mut buffer), Ok(0) | Err(_)));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn connections_past_the_limit_are_answered_503_and_a_free_slot_is_reused() {
    let handle = small(|s| s.max_connections = 1);
    let first = connect(&handle);
    let mut second = connect(&handle);
    let (head, _) = read_reply(&mut second);
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    let mut rest = Vec::new();
    // The over-limit connection is closed after its answer.
    assert_eq!(second.read_to_end(&mut rest).unwrap_or(0), 0);

    drop(first);
    // The slot is free once the relay has seen the first connection go. An answer of `503` can
    // race the request that was sent on the connection it closes, which then reads as a reset.
    let started = Instant::now();
    loop {
        let mut stream = connect(&handle);
        let _ = stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        if answer.starts_with("HTTP/1.1 407") {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "{answer}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_relay_stops_on_shutdown_and_on_drop_joining_its_threads_and_ends_its_tunnels() {
    let origin = StandInOrigin::start("held");
    let handle = relay(direct());
    let address = handle.address();
    let mut connected = tunnel(&handle, &format!("127.0.0.1:{}", origin.addr.port()));
    handle.shutdown();
    // The accept thread has been joined: its listener is closed now, not eventually.
    assert!(TcpStream::connect(address).is_err());
    // The tunnel that was open is ended with its relay.
    let mut rest = String::new();
    let _ = connected.read_to_string(&mut rest);

    let handle = relay(direct());
    let address = handle.address();
    drop(handle);
    assert!(TcpStream::connect(address).is_err());

    // Not waiting for the threads still closes the listener and the connections.
    let handle = small(|s| s.shutdown_wait = Duration::ZERO);
    let address = handle.address();
    let mut idle = connect(&handle);
    drop(handle);
    let mut buffer = [0u8; 8];
    assert!(matches!(idle.read(&mut buffer), Ok(0) | Err(_)));
    let started = Instant::now();
    while TcpStream::connect(address).is_ok() {
        assert!(started.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_token_header_is_accepted_in_place_of_the_login_and_never_forwarded() {
    let origin = StandInOrigin::start("by token");
    let handle = relay(direct());
    let (name, token) = handle.token_header();
    assert_eq!(name, coqui_proxy::relay::TOKEN_HEADER);
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let text =
        format!("GET {url} HTTP/1.1\r\nHost: x\r\n{name}: {token}\r\nConnection: close\r\n\r\n");
    let (head, body) = request(&handle, &text);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "by token");
    assert_eq!(origin.seen()[0].header(name), None);

    let mut stream = connect(&handle);
    stream
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: x\r\n{name}: {token}\r\n\r\n",
                origin.addr.port()
            )
            .as_bytes(),
        )
        .unwrap();
    let (head, _) = read_reply(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");

    let wrong =
        format!("GET {url} HTTP/1.1\r\nHost: x\r\n{name}: {token}x\r\nConnection: close\r\n\r\n");
    let (head, _) = request(&handle, &wrong);
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
    assert_eq!(origin.seen().len(), 1);
}

/// What a body-framing origin received, as text.
type Received = Arc<Mutex<Vec<String>>>;

/// An origin that reads a request with a length or chunked body and answers by path.
struct FramingOrigin {
    addr: SocketAddr,
    received: Received,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FramingOrigin {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let received: Received = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (log, stopped) = (Arc::clone(&received), Arc::clone(&stop));
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let log = Arc::clone(&log);
                thread::spawn(move || answer(stream, &log));
            }
        });
        Self {
            addr,
            received,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FramingOrigin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_line_from(stream: &mut TcpStream) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if stream.read(&mut byte).unwrap_or(0) == 0 {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8_lossy(&line).into_owned()
}

fn answer(mut stream: TcpStream, log: &Received) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request_line = read_line_from(&mut stream);
    let mut headers = Vec::new();
    loop {
        let line = read_line_from(&mut stream);
        if line == "\r\n" || line.is_empty() {
            break;
        }
        headers.push(line.to_ascii_lowercase());
    }
    let length = headers
        .iter()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map(|value| value.trim().parse::<usize>().unwrap());
    let chunked = headers
        .iter()
        .any(|line| line.starts_with("transfer-encoding:") && line.contains("chunked"));
    let mut body = Vec::new();
    if let Some(length) = length {
        body.resize(length, 0);
        stream.read_exact(&mut body).unwrap();
    } else if chunked {
        loop {
            let size = read_line_from(&mut stream);
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            let mut chunk = vec![0u8; size + 2];
            stream.read_exact(&mut chunk).unwrap();
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    }
    let mut entry = format!(
        "{} | {}",
        request_line.trim(),
        String::from_utf8_lossy(&body)
    );
    if chunked {
        entry.push_str(" | chunked");
    }
    log.lock().unwrap().push(entry);
    let path = request_line.split(' ').nth(1).unwrap_or("/").to_string();
    let reply: Vec<u8> = match path.as_str() {
        "/chunked" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".to_vec(),
        "/close" => b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nuntil the end".to_vec(),
        "/head" => b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n".to_vec(),
        "/none" => b"HTTP/1.1 204 No Content\r\n\r\n".to_vec(),
        "/interim" => b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndone".to_vec(),
        _ => {
            let mut reply =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            reply.extend_from_slice(&body);
            reply
        }
    };
    let _ = stream.write_all(&reply);
    let _ = stream.shutdown(Shutdown::Both);
}

/// Reads a whole reply whose body is framed by chunks or by the end of the connection.
fn read_to_end(stream: &mut TcpStream) -> String {
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    reply
}

#[test]
fn request_and_reply_bodies_are_copied_by_length_by_chunks_and_to_the_end() {
    let origin = FramingOrigin::start();
    let handle = relay(direct());
    let base = format!("http://127.0.0.1:{}", origin.addr.port());
    let auth = basic(&handle);

    // A body with a length goes to the origin, and its echo comes back by length.
    let text = format!(
        "POST {base}/echo HTTP/1.1\r\nHost: h\r\nProxy-Authorization: {auth}\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello=world"
    );
    let (head, body) = request(&handle, &text);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "hello=world");

    // A chunked body keeps its chunks to the origin; a chunked reply keeps its chunks back.
    let text = format!(
        "POST {base}/echo HTTP/1.1\r\nHost: h\r\nProxy-Authorization: {auth}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nabcde\r\n3\r\nfgh\r\n0\r\n\r\n"
    );
    let mut stream = connect(&handle);
    stream.write_all(text.as_bytes()).unwrap();
    let (head, body) = read_reply(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "abcdefgh");
    assert!(
        origin
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.ends_with("abcdefgh | chunked")),
        "{:?}",
        origin.received.lock().unwrap()
    );

    let text = format!(
        "GET {base}/chunked HTTP/1.1\r\nHost: h\r\nProxy-Authorization: {auth}\r\nConnection: close\r\n\r\n"
    );
    let mut stream = connect(&handle);
    stream.write_all(text.as_bytes()).unwrap();
    let reply = read_to_end(&mut stream);
    assert!(
        reply
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "{reply}"
    );
    assert!(
        reply.ends_with("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
        "{reply}"
    );

    // A reply with no length ends with the connection, and tells the client so.
    let text =
        format!("GET {base}/close HTTP/1.1\r\nHost: h\r\nProxy-Authorization: {auth}\r\n\r\n");
    let mut stream = connect(&handle);
    stream.write_all(text.as_bytes()).unwrap();
    let reply = read_to_end(&mut stream);
    assert!(
        reply.to_ascii_lowercase().contains("connection: close"),
        "{reply}"
    );
    assert!(reply.ends_with("until the end"), "{reply}");
}

#[test]
fn replies_without_a_body_and_interim_replies_pass_and_the_connection_stays_usable() {
    let origin = FramingOrigin::start();
    let handle = relay(direct());
    let base = format!("http://127.0.0.1:{}", origin.addr.port());
    let auth = basic(&handle);
    let mut stream = connect(&handle);
    let send = |stream: &mut TcpStream, method: &str, path: &str| {
        stream
            .write_all(
                format!(
                    "{method} {base}{path} HTTP/1.1\r\nHost: h\r\nProxy-Authorization: {auth}\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
    };
    // HEAD: the length names the body a GET would have; none follows.
    send(&mut stream, "HEAD", "/head");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    assert!(
        String::from_utf8(head)
            .unwrap()
            .to_ascii_lowercase()
            .contains("content-length: 99")
    );
    // The same connection carries a 204, and a reply that follows an interim one.
    send(&mut stream, "GET", "/none");
    let (head, body) = {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        (String::from_utf8(head).unwrap(), String::new())
    };
    assert!(head.starts_with("HTTP/1.1 204"), "{head} {body}");
    send(&mut stream, "GET", "/interim");
    let (interim, _) = read_reply(&mut stream);
    assert!(interim.starts_with("HTTP/1.1 100"), "{interim}");
    let (head, body) = read_reply(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "done");
}

#[test]
fn a_request_whose_framing_disagrees_with_itself_is_refused_before_it_is_forwarded() {
    let origin = FramingOrigin::start();
    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/echo", origin.addr.port());
    let auth = basic(&handle);
    for framing in [
        "Content-Length: 3\r\nTransfer-Encoding: chunked\r\n",
        "Content-Length: 3\r\nContent-Length: 4\r\n",
        "Transfer-Encoding: gzip\r\n",
        "Content-Length: three\r\n",
    ] {
        let text = format!(
            "POST {url} HTTP/1.1\r\nHost: h\r\nProxy-Authorization: {auth}\r\n{framing}Connection: close\r\n\r\nabc"
        );
        let (head, _) = request(&handle, &text);
        assert!(head.starts_with("HTTP/1.1 400"), "{framing}: {head}");
    }
    assert!(origin.received.lock().unwrap().is_empty());
}

#[test]
fn a_tunnel_carries_bytes_both_ways_and_a_half_close() {
    let origin = FramingOrigin::start();
    let handle = relay(direct());
    let mut stream = tunnel(&handle, &format!("127.0.0.1:{}", origin.addr.port()));
    stream
        .write_all(b"POST /echo HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\n\r\nping")
        .unwrap();
    // The client is done sending; the reply still comes.
    stream.shutdown(Shutdown::Write).unwrap();
    let reply = read_to_end(&mut stream);
    assert!(reply.ends_with("ping"), "{reply}");
}

#[test]
fn requests_that_arrive_with_the_tunnel_request_reach_the_destination() {
    let origin = FramingOrigin::start();
    let handle = relay(direct());
    let target = format!("127.0.0.1:{}", origin.addr.port());
    let mut stream = connect(&handle);
    // The first bytes of the tunnel are sent before the answer to CONNECT is read.
    stream
        .write_all(
            format!(
                "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: {}\r\n\r\nGET /early HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n",
                basic(&handle)
            )
            .as_bytes(),
        )
        .unwrap();
    let reply = read_to_end(&mut stream);
    assert!(
        reply.starts_with("HTTP/1.1 200 OK\r\n\r\nHTTP/1.1 200 OK"),
        "{reply}"
    );
    assert!(origin.received.lock().unwrap()[0].starts_with("GET /early"));
}

#[test]
fn many_requests_at_once_each_get_their_own_answer() {
    let origin = StandInOrigin::start("same for all");
    let handle = Arc::new(relay(direct()));
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let workers: Vec<_> = (0..16)
        .map(|_| {
            let (handle, url) = (Arc::clone(&handle), url.clone());
            thread::spawn(move || {
                for _ in 0..5 {
                    let (head, body) = request(&handle, &get_through(&handle, &url, ""));
                    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
                    assert_eq!(body, "same for all");
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(origin.seen().len(), 80);
}

#[cfg(feature = "tls")]
mod https_proxy {
    use super::*;
    use coqui_proxy::{Proxy, ProxyScheme};

    /// A loopback `https:` proxy: answers one `CONNECT`, then one request in the tunnel, and
    /// closes the TLS stream.
    fn start() -> (u16, Arc<rustls::ClientConfig>) {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params, ca_key);
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            )
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Arc::new(server);
        thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let connection = rustls::ServerConnection::new(server).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, tcp);
            let mut head = Vec::new();
            let mut byte = [0u8];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    return;
                }
                head.push(byte[0]);
            }
            assert!(head.starts_with(b"CONNECT app.internal.test:80 "));
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    return;
                }
                request.push(byte[0]);
            }
            // The reply comes some time after the request, so the relay is polling by then.
            thread::sleep(Duration::from_millis(150));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nvia https",
                )
                .unwrap();
            stream.conn.send_close_notify();
            let _ = stream.flush();
        });
        (port, Arc::new(client))
    }

    #[test]
    fn a_tunnel_over_tls_to_the_proxy_is_polled_and_reaches_the_origin() {
        let (port, config) = start();
        let connector = ProxyConnector::builder().proxy_tls(config).build();
        let proxy = Proxy::builder(ProxyScheme::Https, "localhost", port)
            .build()
            .unwrap();
        let mut settings = RelaySettings::default();
        settings.poll_interval = Duration::from_millis(5);
        let handle = Relay::builder(connector, resolver("ALL_PROXY", proxy.to_string()))
            .settings(settings)
            .start()
            .unwrap();
        let mut connected = tunnel(&handle, "app.internal.test:80");
        assert!(get_in(&mut connected, "app.internal.test").ends_with("via https"));
    }
}

#[test]
fn a_body_may_arrive_more_slowly_than_a_head_may() {
    let origin = FramingOrigin::start();
    let handle = small(|s| {
        s.header_timeout = Duration::from_millis(300);
        s.idle_timeout = Duration::from_secs(60);
    });
    let url = format!("http://127.0.0.1:{}/echo", origin.addr.port());
    let mut stream = connect(&handle);
    // The head comes in two parts, so the time left for it is what the second read waits with.
    let first = format!(
        "POST {url} HTTP/1.1\r\nHost: h\r\nProxy-Authorization: {}\r\nContent-Length: 5\r\n",
        basic(&handle)
    );
    stream.write_all(first.as_bytes()).unwrap();
    thread::sleep(Duration::from_millis(100));
    stream.write_all(b"Connection: close\r\n\r\nsl").unwrap();
    // The head was complete long ago: only the idle time bounds the rest of the body.
    thread::sleep(Duration::from_millis(900));
    stream.write_all(b"owl").unwrap();
    let (head, body) = read_reply(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "slowl");
}

#[test]
fn a_connection_that_sends_no_request_is_closed_after_the_header_time() {
    let handle = small(|s| {
        s.header_timeout = Duration::from_millis(300);
        s.idle_timeout = Duration::from_secs(60);
    });
    let mut stream = connect(&handle);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let started = Instant::now();
    let mut buffer = [0u8; 16];
    let read = stream.read(&mut buffer);
    assert!(matches!(read, Ok(0)), "{read:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}
