//! Real proxies as peers: squid (HTTP, Basic login) and dante (SOCKS 5), and libproxy. Each test
//! is ignored unless asked for, and needs its program installed:
//!
//! - `COQUI_PROXY_TEST_SQUID=1` with `squid` and `htpasswd`,
//! - `COQUI_PROXY_TEST_DANTE=1` with `sockd`,
//! - `COQUI_PROXY_TEST_LIBPROXY=1` with libproxy (feature `libproxy`).
//!
//! Run with `cargo test --test real_peers -- --ignored`. Configuration, password files and logs
//! live in a temporary directory; both servers listen on loopback ports chosen by the test.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use coqui_proxy::{Credentials, Error, Proxy, ProxyConnector, ProxyScheme, Route};

fn wanted(variable: &str) -> bool {
    let wanted = std::env::var_os(variable).is_some();
    if !wanted {
        eprintln!("skipped: set {variable}=1");
    }
    wanted
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A server program that is stopped when the value is dropped.
struct Daemon(Child);

impl Daemon {
    fn start(mut command: Command, port: u16) -> Self {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server program starts");
        let daemon = Self(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "the server did not listen");
            thread::sleep(Duration::from_millis(100));
        }
        daemon
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A loopback origin that answers each connection with `hello` and closes.
fn origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let _ = stream.write_all(b"hello");
        }
    });
    port
}

fn read_hello(connection: &mut impl Read) -> String {
    let mut text = String::new();
    connection
        .take(5)
        .read_to_string(&mut text)
        .expect("the origin answers through the proxy");
    text
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

fn squid(directory: &Path, port: u16) -> Daemon {
    let passwords = directory.join("htpasswd");
    let made = Command::new("htpasswd")
        .args(["-b", "-B", "-c"])
        .arg(&passwords)
        .args(["ada", "s3cret"])
        .output()
        .expect("htpasswd runs");
    assert!(made.status.success(), "htpasswd failed");
    let helper = ["/usr/lib64/squid", "/usr/lib/squid", "/usr/libexec/squid"]
        .into_iter()
        .map(|dir| Path::new(dir).join("basic_ncsa_auth"))
        .find(|path| path.exists())
        .expect("squid's basic_ncsa_auth helper");
    let dir = directory.display();
    write(
        &directory.join("squid.conf"),
        &format!(
            "http_port 127.0.0.1:{port}\n\
             pid_filename {dir}/squid.pid\n\
             cache_log {dir}/cache.log\n\
             access_log none\n\
             cache deny all\n\
             coredump_dir {dir}\n\
             auth_param basic program {} {}\n\
             auth_param basic realm test\n\
             acl authed proxy_auth REQUIRED\n\
             http_access allow authed\n\
             http_access deny all\n",
            helper.display(),
            passwords.display()
        ),
    );
    let mut command = Command::new("squid");
    command
        .arg("-N")
        .arg("-f")
        .arg(directory.join("squid.conf"));
    Daemon::start(command, port)
}

fn proxy_with(scheme: ProxyScheme, port: u16, credentials: Option<Credentials>) -> Route {
    let mut builder = Proxy::builder(scheme, "127.0.0.1", port);
    if let Some(credentials) = credentials {
        builder = builder.credentials(credentials);
    }
    Route::Proxy(builder.build().unwrap())
}

#[test]
#[ignore = "needs squid"]
fn squid_tunnels_for_a_user_with_the_right_password() {
    if !wanted("COQUI_PROXY_TEST_SQUID") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let port = free_port();
    let _squid = squid(directory.path(), port);
    let origin = origin();

    let route = proxy_with(
        ProxyScheme::Http,
        port,
        Some(Credentials::new("ada", "s3cret")),
    );
    let mut connection = ProxyConnector::default()
        .connect(&route, "127.0.0.1", origin)
        .expect("a tunnel");
    assert_eq!(read_hello(&mut connection), "hello");
}

#[test]
#[ignore = "needs squid"]
fn squid_refuses_a_wrong_password_and_a_missing_one_as_login_errors() {
    if !wanted("COQUI_PROXY_TEST_SQUID") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let port = free_port();
    let _squid = squid(directory.path(), port);
    let origin = origin();
    let connector = ProxyConnector::default();

    let wrong = proxy_with(
        ProxyScheme::Http,
        port,
        Some(Credentials::new("ada", "wrong")),
    );
    let error = connector.connect(&wrong, "127.0.0.1", origin).unwrap_err();
    assert!(matches!(error, Error::Login(_)), "{error}");
    assert!(!error.to_string().contains("wrong"), "{error}");

    let none = proxy_with(ProxyScheme::Http, port, None);
    let error = connector.connect(&none, "127.0.0.1", origin).unwrap_err();
    assert!(matches!(error, Error::Login(_)), "{error}");
}

#[test]
#[ignore = "needs dante"]
fn dante_tunnels_socks5_and_socks4() {
    if !wanted("COQUI_PROXY_TEST_DANTE") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let port = free_port();
    let user = std::env::var("USER").unwrap_or_else(|_| "nobody".to_string());
    write(
        &directory.path().join("sockd.conf"),
        &format!(
            "logoutput: stderr\n\
             internal: 127.0.0.1 port = {port}\n\
             external: lo\n\
             socksmethod: none\n\
             clientmethod: none\n\
             user.privileged: {user}\n\
             user.unprivileged: {user}\n\
             client pass {{ from: 127.0.0.0/8 to: 0.0.0.0/0 }}\n\
             socks pass {{ from: 127.0.0.0/8 to: 0.0.0.0/0 command: connect }}\n"
        ),
    );
    let mut command = Command::new("sockd");
    command
        .arg("-f")
        .arg(directory.path().join("sockd.conf"))
        .arg("-p")
        .arg(directory.path().join("sockd.pid"));
    let _dante = Daemon::start(command, port);
    let origin = origin();
    let connector = ProxyConnector::default();

    for scheme in [ProxyScheme::Socks5, ProxyScheme::Socks4] {
        let route = proxy_with(scheme, port, None);
        let mut connection = connector
            .connect(&route, "127.0.0.1", origin)
            .unwrap_or_else(|error| panic!("{scheme}: {error}"));
        assert_eq!(read_hello(&mut connection), "hello", "{scheme}");
    }
}

#[cfg(feature = "libproxy")]
#[test]
#[ignore = "needs libproxy"]
fn libproxy_answers_from_the_environment_of_the_process() {
    use coqui_proxy::ProxySource;
    use coqui_proxy::system::LibProxySource;

    if !wanted("COQUI_PROXY_TEST_LIBPROXY") {
        return;
    }
    let source = LibProxySource::load(None::<&str>).expect("libproxy loads");
    let routes = source
        .routes("http://example.com/")
        .expect("an answer")
        .expect("an opinion");
    assert!(!routes.is_empty());
    // The runner sets `http_proxy=http://libproxy-test.example:3128` for this test.
    if std::env::var("http_proxy").as_deref() == Ok("http://libproxy-test.example:3128") {
        assert!(matches!(
            &routes[0],
            Route::Proxy(proxy) if proxy.host() == "libproxy-test.example" && proxy.port() == 3128
        ));
    }
}
