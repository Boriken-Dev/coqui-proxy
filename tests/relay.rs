#![cfg(feature = "relay")]

//! The in-process relay: `CONNECT` and absolute-form `http:` requests sent through the
//! connector's routes (failover, login, direct), its credential, and its bounds.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use coqui_proxy::relay::{Relay, RelayHandle};
use coqui_proxy::{
    Credentials, Environment, Error, ProxyConnector as Blocking, RelaySettings, Resolver,
};
use support::socks_server::SocksServer;
use support::{Require, StandInOrigin, StandInProxy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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
    Relay::builder(Blocking::default().asynch(), resolver)
        .start()
        .unwrap()
}

fn dead_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A listener that accepts and drops every connection, counting them, on a port that was dead
/// before: a connector's attempt on it is seen.
struct Flaky {
    attempts: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Flaky {
    async fn on(port: u16) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        Self { attempts, task }
    }
}

impl Drop for Flaky {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn connect(handle: &RelayHandle) -> TcpStream {
    TcpStream::connect(handle.address()).await.unwrap()
}

/// Reads one reply: its head, and a body of `Content-Length` bytes.
async fn read_reply(stream: &mut TcpStream) -> (String, String) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await.unwrap());
    }
    let head = String::from_utf8(head).unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await.unwrap();
    (head, String::from_utf8(body).unwrap())
}

async fn request(handle: &RelayHandle, text: &str) -> (String, String) {
    let mut stream = connect(handle).await;
    stream.write_all(text.as_bytes()).await.unwrap();
    read_reply(&mut stream).await
}

fn get_through(handle: &RelayHandle, url: &str, extra: &str) -> String {
    let host = url.split('/').nth(2).unwrap();
    format!(
        "GET {url} HTTP/1.1\r\nHost: {host}\r\nProxy-Authorization: {}\r\n{extra}Connection: close\r\n\r\n",
        basic(handle)
    )
}

/// Sends `CONNECT`, checks the answer and returns the tunnel.
async fn tunnel(handle: &RelayHandle, target: &str) -> TcpStream {
    let mut stream = connect(handle).await;
    stream
        .write_all(
            format!(
                "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: {}\r\n\r\n",
                basic(handle)
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let (head, _) = read_reply(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    stream
}

async fn get_in(stream: &mut TcpStream, host: &str) -> String {
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply).await;
    reply
}

#[tokio::test]
async fn both_kinds_of_request_fail_over_from_a_dead_proxy_and_skip_it_afterwards() {
    let origin = StandInOrigin::start("via the second");
    let live = StandInProxy::start(Require::Nothing, origin.addr);
    let dead = dead_port();
    let handle = relay(resolver(
        "ALL_PROXY",
        format!("PROXY 127.0.0.1:{dead}; PROXY {}", live.addr),
    ));

    let mut connected = tunnel(&handle, "app.internal.test:8443").await;
    assert!(
        get_in(&mut connected, "app.internal.test")
            .await
            .ends_with("via the second")
    );

    // Something now listens where the dead proxy was: a second attempt on it would be counted.
    let listening = Flaky::on(dead).await;
    let (head, body) = request(
        &handle,
        &get_through(&handle, "http://app.internal.test/data?x=1", ""),
    )
    .await;
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
    )
    .await;
    assert!(
        head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.1 502"),
        "{head}"
    );
    assert!(listening.attempts.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn requests_go_through_a_proxy_that_wants_a_login() {
    let origin = StandInOrigin::start("behind a login");
    let live = StandInProxy::start(Require::Basic("ada", "s3cret"), origin.addr);
    let handle = relay(resolver("ALL_PROXY", live.proxy_url()));

    let mut connected = tunnel(&handle, "app.internal.test:8443").await;
    assert!(
        get_in(&mut connected, "app.internal.test")
            .await
            .ends_with("behind a login")
    );
    let (head, body) = request(
        &handle,
        &get_through(&handle, "http://app.internal.test/", ""),
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "behind a login");
    for seen in live.seen() {
        assert_eq!(
            seen.header("proxy-authorization"),
            Some("Basic YWRhOnMzY3JldA==")
        );
    }
}

#[tokio::test]
async fn requests_go_through_a_socks_proxy_and_direct() {
    let origin = StandInOrigin::start("the origin");
    let socks = SocksServer::start(origin.addr, None);
    let handle = relay(resolver("ALL_PROXY", format!("socks5://{}", socks.addr)));
    let (_, body) = request(&handle, &get_through(&handle, "http://mail.test/", "")).await;
    assert_eq!(body, "the origin");
    assert_eq!(socks.asked()[0].destination, "mail.test");

    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/direct", origin.addr.port());
    let (head, body) = request(&handle, &get_through(&handle, &url, "")).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "the origin");
    let mut connected = tunnel(&handle, &format!("127.0.0.1:{}", origin.addr.port())).await;
    assert!(
        get_in(&mut connected, "127.0.0.1")
            .await
            .ends_with("the origin")
    );
}

#[tokio::test]
async fn a_request_is_forwarded_in_origin_form_without_hop_by_hop_headers() {
    let origin = StandInOrigin::start("seen");
    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/path/to?q=1&r=2", origin.addr.port());
    let extra = "X-Kept: yes\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nX-Secret: hidden\r\nTE: trailers\r\n";
    let text = get_through(&handle, &url, extra)
        .replace("Connection: close", "Connection: close, X-Secret");
    let (head, _) = request(&handle, &text).await;
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

#[tokio::test]
async fn the_clients_connection_is_kept_alive_for_the_next_request() {
    let origin = StandInOrigin::start("again");
    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let mut stream = connect(&handle).await;
    for _ in 0..2 {
        let text = get_through(&handle, &url, "").replace("Connection: close\r\n", "");
        stream.write_all(text.as_bytes()).await.unwrap();
        let (head, body) = read_reply(&mut stream).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, "again");
    }
    assert_eq!(origin.seen().len(), 2);
}

#[tokio::test]
async fn a_request_without_the_credential_or_with_a_wrong_one_is_refused() {
    let origin = StandInOrigin::start("never");
    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let anonymous = format!("GET {url} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let (head, _) = request(&handle, &anonymous).await;
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("proxy-authenticate: basic")
    );

    let wrong = base64::engine::general_purpose::STANDARD.encode("coqui-relay:not-the-password");
    let text = format!(
        "GET {url} HTTP/1.1\r\nHost: x\r\nProxy-Authorization: Basic {wrong}\r\nConnection: close\r\n\r\n"
    );
    let (head, _) = request(&handle, &text).await;
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");

    // A tunnel is refused the same way, and nothing reached the origin.
    let (head, _) = request(
        &handle,
        &format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            origin.addr.port()
        ),
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
    assert!(origin.seen().is_empty());

    let (head, _) = request(&handle, &get_through(&handle, &url, "")).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

#[tokio::test]
async fn a_destination_no_route_reaches_is_502_and_anything_else_is_405() {
    let handle = relay(resolver(
        "ALL_PROXY",
        format!("socks5://127.0.0.1:{}", dead_port()),
    ));
    let (head, body) = request(&handle, &get_through(&handle, "http://app.test/", "")).await;
    assert!(head.starts_with("HTTP/1.1 502"), "{head}");
    assert!(body.contains("no reachable proxy"), "{body}");
    let (head, _) = request(
        &handle,
        &format!(
            "CONNECT app.test:443 HTTP/1.1\r\nHost: app.test\r\nProxy-Authorization: {}\r\nConnection: close\r\n\r\n",
            basic(&handle)
        ),
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 502"), "{head}");

    let origin_form = format!(
        "GET / HTTP/1.1\r\nHost: x\r\nProxy-Authorization: {}\r\nConnection: close\r\n\r\n",
        basic(&handle)
    );
    let (head, _) = request(&handle, &origin_form).await;
    assert!(head.starts_with("HTTP/1.1 405"), "{head}");
    let https = get_through(&handle, "https://app.test/", "");
    let (head, _) = request(&handle, &https).await;
    assert!(head.starts_with("HTTP/1.1 405"), "{head}");
}

#[tokio::test]
async fn the_credential_is_random_for_each_relay_and_never_printed() {
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
        format!(
            "{:?}",
            Relay::builder(Blocking::default().asynch(), direct())
        ),
    ] {
        assert!(!text.contains(&password), "{text}");
    }
    assert_eq!(
        first.proxy_url(),
        format!("http://127.0.0.1:{}", first.address().port())
    );
    assert!(first.address().ip().is_loopback());
}

#[tokio::test]
async fn the_settings_that_are_not_meaningful_or_not_loopback_are_refused() {
    let build = |change: &dyn Fn(&mut RelaySettings)| {
        let mut settings = RelaySettings::default();
        change(&mut settings);
        Relay::builder(Blocking::default().asynch(), direct())
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
    assert!(build(&|s| s.max_head_bytes = 100).is_err());
    assert!(build(&|_| {}).is_ok());

    let mut loose = RelaySettings::default();
    loose.max_connections *= 2;
    loose.idle_timeout *= 2;
    let builder = Relay::builder(Blocking::default().asynch(), direct()).settings(loose);
    assert_eq!(builder.non_defaults().len(), 2);
    assert!(
        Relay::builder(Blocking::default().asynch(), direct())
            .non_defaults()
            .is_empty()
    );
}

fn small(change: impl FnOnce(&mut RelaySettings)) -> RelayHandle {
    let mut settings = RelaySettings::default();
    change(&mut settings);
    Relay::builder(Blocking::default().asynch(), direct())
        .settings(settings)
        .start()
        .unwrap()
}

#[tokio::test]
async fn a_head_longer_than_the_limit_is_refused() {
    let handle = small(|s| s.max_head_bytes = RelaySettings::MIN_MAX_HEAD_BYTES);
    let mut stream = connect(&handle).await;
    let long = "a".repeat(RelaySettings::MIN_MAX_HEAD_BYTES * 2);
    let text = format!(
        "GET http://x.test/ HTTP/1.1\r\nHost: x.test\r\nX-Long: {long}\r\nProxy-Authorization: {}\r\n\r\n",
        basic(&handle)
    );
    let _ = stream.write_all(text.as_bytes()).await;
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply).await;
    assert!(
        reply.is_empty() || reply.starts_with("HTTP/1.1 431"),
        "{reply}"
    );
    assert!(!reply.contains("200"), "{reply}");
    // The relay still serves.
    let (head, _) = request(
        &handle,
        "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
}

#[tokio::test]
async fn a_silent_connection_is_closed_after_the_idle_time() {
    let handle = small(|s| s.idle_timeout = Duration::from_millis(300));
    let mut stream = connect(&handle).await;
    let started = Instant::now();
    let mut buffer = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer))
        .await
        .expect("the relay closed the connection");
    assert!(matches!(read, Ok(0) | Err(_)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_slow_head_is_cut_off_after_the_header_time() {
    let handle = small(|s| {
        s.header_timeout = Duration::from_millis(300);
        s.idle_timeout = Duration::from_secs(60);
    });
    let mut stream = connect(&handle).await;
    stream
        .write_all(b"GET http://x.test/ HTTP/1.1\r\nHost: x")
        .await
        .unwrap();
    let started = Instant::now();
    let mut buffer = [0u8; 256];
    let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer))
        .await
        .expect("the relay closed the connection");
    assert!(matches!(read, Ok(_) | Err(_)));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn connections_past_the_limit_wait_for_a_free_slot() {
    let handle = small(|s| s.max_connections = 1);
    let first = connect(&handle).await;
    let mut second = connect(&handle).await;
    second
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buffer = [0u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(400), second.read(&mut buffer))
            .await
            .is_err(),
        "the second connection was served while the first held the only slot"
    );
    drop(first);
    let (head, _) = tokio::time::timeout(Duration::from_secs(10), read_reply(&mut second))
        .await
        .expect("the second connection is served once the slot is free");
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
}

async fn refuses(address: SocketAddr) -> bool {
    for _ in 0..50 {
        if TcpStream::connect(address).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn a_relay_stops_on_shutdown_and_on_drop_and_ends_its_tunnels() {
    let origin = StandInOrigin::start("held");
    let handle = relay(direct());
    let address = handle.address();
    let mut connected = tunnel(&handle, &format!("127.0.0.1:{}", origin.addr.port())).await;
    handle.shutdown().await;
    assert!(TcpStream::connect(address).await.is_err());
    // The tunnel that was open is ended with its relay.
    let mut rest = String::new();
    let read = tokio::time::timeout(Duration::from_secs(10), connected.read_to_string(&mut rest))
        .await
        .expect("the tunnel ended");
    let _ = read;

    let handle = relay(direct());
    let address = handle.address();
    drop(handle);
    assert!(refuses(address).await);
}

#[tokio::test]
async fn starting_outside_a_runtime_is_an_error_not_a_panic() {
    let connector = Blocking::default().asynch();
    let resolver = direct();
    let outcome = std::thread::spawn(move || Relay::builder(connector, resolver).start().err())
        .join()
        .unwrap();
    assert!(matches!(outcome, Some(Error::Io(_))), "{outcome:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn the_relay_runs_on_a_current_thread_runtime() {
    let origin = StandInOrigin::start("single");
    let handle = relay(direct());
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let (_, body) = request(&handle, &get_through(&handle, &url, "")).await;
    assert_eq!(body, "single");
}

#[tokio::test]
async fn the_token_header_is_accepted_in_place_of_the_login_and_never_forwarded() {
    let origin = StandInOrigin::start("by token");
    let handle = relay(direct());
    let (name, token) = handle.token_header();
    assert_eq!(name, coqui_proxy::relay::TOKEN_HEADER);
    let url = format!("http://127.0.0.1:{}/", origin.addr.port());
    let text =
        format!("GET {url} HTTP/1.1\r\nHost: x\r\n{name}: {token}\r\nConnection: close\r\n\r\n");
    let (head, body) = request(&handle, &text).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "by token");
    assert_eq!(origin.seen()[0].header(name), None);

    let mut stream = connect(&handle).await;
    stream
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: x\r\n{name}: {token}\r\n\r\n",
                origin.addr.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let (head, _) = read_reply(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");

    let wrong =
        format!("GET {url} HTTP/1.1\r\nHost: x\r\n{name}: {token}x\r\nConnection: close\r\n\r\n");
    let (head, _) = request(&handle, &wrong).await;
    assert!(head.starts_with("HTTP/1.1 407"), "{head}");
    assert_eq!(origin.seen().len(), 1);
}
