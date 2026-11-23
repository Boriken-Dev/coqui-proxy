#![cfg(feature = "tokio")]

mod support;

use coqui_proxy::asynch::{self, ProxyConnector};
use coqui_proxy::{
    Credentials, Environment, Error, Proxy, ProxyConnector as Blocking, ProxyScheme, Resolver,
    Route,
};
use support::socks_server::SocksServer;
use support::{Require, StandInOrigin, StandInProxy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn get(stream: &mut (impl AsyncReadExt + AsyncWriteExt + Unpin), host: &str) -> String {
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

fn http_proxy(port: u16, login: Option<(&str, &str)>) -> Route {
    let mut builder = Proxy::builder(ProxyScheme::Http, "127.0.0.1", port);
    if let Some((user, password)) = login {
        builder = builder.credentials(Credentials::new(user, password));
    }
    Route::Proxy(builder.build().unwrap())
}

fn dead_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn a_connect_with_basic_credentials_reaches_the_origin() {
    let origin = StandInOrigin::start("hello from the origin");
    let proxy = StandInProxy::start(Require::Basic("ada", "s3cret"), origin.addr);
    let route = http_proxy(proxy.addr.port(), Some(("ada", "s3cret")));
    let mut connection = ProxyConnector::default()
        .connect(&route, "app.internal.test", 80)
        .await
        .unwrap();
    assert!(
        get(&mut connection, "app.internal.test")
            .await
            .ends_with("hello from the origin")
    );
    let seen = proxy.seen();
    assert_eq!(
        seen[0].request_line,
        "CONNECT app.internal.test:80 HTTP/1.1"
    );
    assert_eq!(
        seen[0].header("proxy-authorization"),
        Some("Basic YWRhOnMzY3JldA==")
    );
}

#[tokio::test]
async fn a_refused_login_is_a_login_error_that_does_not_echo_the_password() {
    let origin = StandInOrigin::start("x");
    let proxy = StandInProxy::start(Require::Basic("ada", "s3cret"), origin.addr);
    let route = http_proxy(proxy.addr.port(), Some(("ada", "wrong")));
    let error = ProxyConnector::default()
        .connect(&route, "app.internal.test", 80)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Login(_)), "{error}");
    assert!(!error.to_string().contains("wrong"));
    let route = http_proxy(proxy.addr.port(), None);
    let error = ProxyConnector::default()
        .connect(&route, "app.internal.test", 80)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Login(_)), "{error}");
}

#[tokio::test]
async fn a_proxy_header_goes_in_the_connect() {
    let origin = StandInOrigin::start("tunnelled");
    let proxy = StandInProxy::start(Require::Header("X-Proxy-Token", "abc"), origin.addr);
    let built = Proxy::builder(ProxyScheme::Http, "127.0.0.1", proxy.addr.port())
        .header("X-Proxy-Token", "abc")
        .build()
        .unwrap();
    let mut connection = ProxyConnector::default()
        .connect(&Route::Proxy(built), "x.test", 80)
        .await
        .unwrap();
    assert!(get(&mut connection, "x.test").await.ends_with("tunnelled"));
}

#[tokio::test]
async fn socks_4a_and_5_tunnel_and_log_in() {
    let origin = StandInOrigin::start("through socks");
    let server = SocksServer::start(origin.addr, Some(("ada", "s3cret")));
    let socks = |scheme, login: Option<(&str, &str)>| {
        let mut builder = Proxy::builder(scheme, "127.0.0.1", server.addr.port());
        if let Some((user, password)) = login {
            builder = builder.credentials(Credentials::new(user, password));
        }
        Route::Proxy(builder.build().unwrap())
    };
    let connector = ProxyConnector::default();
    let route = socks(ProxyScheme::Socks5, Some(("ada", "s3cret")));
    let mut connection = connector
        .connect(&route, "mail.internal.test", 80)
        .await
        .unwrap();
    assert!(get(&mut connection, "x").await.ends_with("through socks"));
    let asked = server.asked();
    assert_eq!(asked[0].destination, "mail.internal.test");
    assert!(asked[0].name);
    let error = connector
        .connect(
            &socks(ProxyScheme::Socks5, Some(("ada", "wrong"))),
            "x.test",
            80,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Login(_)), "{error}");
    assert!(!error.to_string().contains("wrong"));

    let open = SocksServer::start(origin.addr, None);
    let route = Route::Proxy(
        Proxy::builder(ProxyScheme::Socks4, "127.0.0.1", open.addr.port())
            .credentials(Credentials::new("ada", "unused"))
            .build()
            .unwrap(),
    );
    let mut connection = connector
        .connect(&route, "mail.internal.test", 80)
        .await
        .unwrap();
    assert!(get(&mut connection, "x").await.ends_with("through socks"));
    let asked = open.asked();
    assert!(asked[0].name);
    assert_eq!(asked[0].user.as_deref(), Some("ada"));
}

#[tokio::test]
async fn a_direct_route_and_a_name_with_two_address_families_connect() {
    let origin = StandInOrigin::start("direct");
    let connector = ProxyConnector::default();
    let mut connection = connector
        .connect(&Route::Direct, "127.0.0.1", origin.addr.port())
        .await
        .unwrap();
    assert!(get(&mut connection, "127.0.0.1").await.ends_with("direct"));
    // `localhost` may name ::1 first; the origin listens on IPv4 only.
    let mut connection = connector
        .connect(&Route::Direct, "localhost", origin.addr.port())
        .await
        .unwrap();
    assert!(get(&mut connection, "localhost").await.ends_with("direct"));
    let error = connector
        .connect(&Route::Direct, "127.0.0.1", dead_port())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error}");
}

fn socks_resolver(list: String) -> Resolver {
    Resolver::builder()
        .environment(Environment::from_pairs([("ALL_PROXY", list)]))
        .build()
}

#[tokio::test]
async fn connect_url_fails_over_and_remembers_the_proxy_that_failed() {
    let origin = StandInOrigin::start("via the second");
    let live = SocksServer::start(origin.addr, None);
    let dead = dead_port();
    let resolver = socks_resolver(format!("SOCKS5 127.0.0.1:{dead}; SOCKS5 {}", live.addr));
    let connector = ProxyConnector::default();
    let mut connection = connector
        .connect_url(&resolver, "http://app.internal.test/")
        .await
        .unwrap();
    assert!(
        get(&mut connection, "app.internal.test")
            .await
            .ends_with("via the second")
    );
    let routes = resolver.lookup("http://app.internal.test/").unwrap();
    assert!(matches!(routes.last(), Some(Route::Proxy(p)) if p.port() == dead));
}

#[tokio::test]
async fn failover_off_tries_only_the_first_route_and_the_connector_keeps_its_own_memory() {
    let origin = StandInOrigin::start("second");
    let live = SocksServer::start(origin.addr, None);
    let dead = dead_port();
    let list = format!("SOCKS5 127.0.0.1:{dead}; SOCKS5 {}", live.addr);
    let once = Blocking::builder().failover(false).build();
    assert_eq!(once.non_defaults().len(), 1);
    let error = once
        .asynch()
        .connect_url(&socks_resolver(list.clone()), "http://app.internal.test/")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NoReachableProxy), "{error}");

    // The connector's own memory orders its attempts whichever resolver it is given.
    let shared = Blocking::default().asynch();
    shared
        .connect_url(&socks_resolver(list.clone()), "http://a.test/")
        .await
        .unwrap();
    shared
        .connect_url(&socks_resolver(list), "http://a.test/")
        .await
        .unwrap();
    assert_eq!(live.asked().len(), 2);
}

#[tokio::test]
async fn the_connector_and_its_futures_are_send_so_a_task_can_own_them() {
    let origin = StandInOrigin::start("sent");
    let server = SocksServer::start(origin.addr, None);
    let resolver = socks_resolver(format!("SOCKS5 {}", server.addr));
    let connector = ProxyConnector::default();
    let task = tokio::spawn(async move {
        let mut connection = connector
            .connect_url(&resolver, "http://app.internal.test/")
            .await
            .unwrap();
        get(&mut connection, "app.internal.test").await
    });
    assert!(task.await.unwrap().ends_with("sent"));
}

#[tokio::test]
async fn lookup_asks_the_resolver_on_the_blocking_pool() {
    let resolver = Resolver::builder()
        .environment(Environment::from_pairs([(
            "HTTP_PROXY",
            "http://proxy.test:3128",
        )]))
        .build();
    let routes = asynch::lookup(&resolver, "http://app.internal.test/")
        .await
        .unwrap();
    assert!(matches!(&routes[0], Route::Proxy(p) if p.host() == "proxy.test"));
}

#[tokio::test(flavor = "current_thread")]
async fn it_works_on_a_current_thread_runtime() {
    let origin = StandInOrigin::start("one thread");
    let mut connection = ProxyConnector::default()
        .connect(&Route::Direct, "127.0.0.1", origin.addr.port())
        .await
        .unwrap();
    assert!(get(&mut connection, "x").await.ends_with("one thread"));
}

#[cfg(feature = "hyper")]
mod hyper_connector {
    use super::*;
    use coqui_proxy::hyper::Connector;
    use tower_service::Service;

    #[tokio::test]
    async fn the_service_returns_a_tunnel_through_the_first_route_that_connects() {
        let origin = StandInOrigin::start("served");
        let proxy = StandInProxy::start(Require::Basic("ada", "s3cret"), origin.addr);
        let resolver = Resolver::builder()
            .environment(Environment::from_pairs([(
                "HTTP_PROXY",
                format!(
                    "PROXY 127.0.0.1:{}; PROXY 127.0.0.1:{}",
                    dead_port(),
                    proxy.addr.port()
                ),
            )]))
            .credentials(Credentials::new("ada", "s3cret"))
            .build();
        let mut service = Connector::new(Blocking::default().asynch(), resolver);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            service.poll_ready(&mut context),
            std::task::Poll::Ready(Ok(()))
        ));
        let tunnel = service
            .call("http://app.internal.test/".parse().unwrap())
            .await
            .unwrap();
        let mut connection = tunnel.into_inner();
        assert!(
            get(&mut connection, "app.internal.test")
                .await
                .ends_with("served")
        );
    }

    #[tokio::test]
    async fn a_uri_without_a_host_is_an_error() {
        let mut service = Connector::new(Blocking::default().asynch(), Resolver::system());
        let error = service
            .call("/just/a/path".parse().unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Parse(_)), "{error}");
    }
}

#[cfg(feature = "tls")]
mod https_proxy {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;

    /// A loopback `https:` proxy: accepts one `CONNECT`, answers it and then serves a fixed reply.
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
        std::thread::spawn(move || {
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

    #[tokio::test]
    async fn a_connect_over_tls_to_the_proxy_reaches_the_origin() {
        let (port, config) = start();
        let connector: ProxyConnector = Blocking::builder().proxy_tls(config).build().asynch();
        let route = Route::Proxy(
            Proxy::builder(ProxyScheme::Https, "localhost", port)
                .build()
                .unwrap(),
        );
        let mut connection = connector
            .connect(&route, "app.internal.test", 80)
            .await
            .unwrap();
        assert!(
            get(&mut connection, "app.internal.test")
                .await
                .ends_with("via https")
        );
    }
}
