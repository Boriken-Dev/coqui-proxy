//! The PAC pieces proxylib has: file and URL scripts, encodings, custom builtins, browser
//! compatibility, the builtin list, and a script that is downloaded again when it is stale.

#![cfg(feature = "pac")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use coqui_proxy::pac::{PacEncoding, PacScript, PacSource, builtin_names, discover};
use coqui_proxy::{
    Environment, Error, PacSettings, ProxySetting, ProxySource, Resolver, ResolverSettings, Route,
};

const SCRIPT: &str = "function FindProxyForURL(url, host) { return 'PROXY ' + host + ':8080'; }";

fn serve_script(counter: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            counter.fetch_add(1, Ordering::SeqCst);
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SCRIPT}",
                SCRIPT.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    port
}

#[test]
fn every_builtin_name_is_a_function_in_a_script() {
    let checks: String = builtin_names()
        .iter()
        .map(|name| format!("if (typeof {name} !== 'function') return '{name}';"))
        .collect();
    let source = format!("function FindProxyForURL() {{ {checks} return 'DIRECT'; }}");
    let script = PacScript::builder(source).build().unwrap();
    assert_eq!(
        script.find_proxy_for_url("http://a/", "a").unwrap(),
        "DIRECT"
    );
    assert!(builtin_names().contains(&"dnsResolveEx"));
}

#[test]
fn a_custom_function_is_a_builtin_and_its_error_is_a_script_error() {
    let source = "function FindProxyForURL(url, host) { \
                  if (hostIsBlocked(host)) { return 'PROXY block.example:1'; } \
                  return upper(host, 'x') ; }";
    let script = PacScript::builder(source)
        .function("hostIsBlocked", |arguments| {
            Ok(if arguments[0] == "bad.example" {
                "yes"
            } else {
                ""
            }
            .to_string())
        })
        .function("upper", |arguments| {
            if arguments.len() == 2 {
                Ok(arguments[0].to_uppercase())
            } else {
                Err(Error::Parse("arguments"))
            }
        })
        .build()
        .unwrap();
    assert_eq!(
        script
            .find_proxy_for_url("http://bad.example/", "bad.example")
            .unwrap(),
        "PROXY block.example:1"
    );
    assert_eq!(
        script
            .find_proxy_for_url("http://ok.example/", "ok.example")
            .unwrap(),
        "OK.EXAMPLE"
    );
}

#[test]
fn a_file_is_read_as_utf8_or_latin1() {
    let directory = tempfile::tempdir().unwrap();
    let utf8 = directory.path().join("utf8.pac");
    let mut text = vec![0xEF, 0xBB, 0xBF];
    text.extend_from_slice(
        "function FindProxyForURL() { return 'PROXY caf\u{e9}.example:1'; }".as_bytes(),
    );
    std::fs::write(&utf8, &text).unwrap();
    let settings = PacSettings::default();
    let script = PacScript::from_file(&utf8, PacEncoding::Utf8, &settings).unwrap();
    assert_eq!(
        script.find_proxy_for_url("http://a/", "a").unwrap(),
        "PROXY caf\u{e9}.example:1"
    );
    assert_eq!(script.timeout(), Duration::from_secs(5));
    assert!(script.source().starts_with("function"));

    let latin1 = directory.path().join("latin1.pac");
    std::fs::write(
        &latin1,
        b"function FindProxyForURL() { return 'PROXY caf\xe9.example:1'; }",
    )
    .unwrap();
    assert!(PacScript::from_file(&latin1, PacEncoding::Utf8, &settings).is_err());
    let script = PacScript::from_file(&latin1, PacEncoding::Latin1, &settings).unwrap();
    assert_eq!(
        script.find_proxy_for_url("http://a/", "a").unwrap(),
        "PROXY caf\u{e9}.example:1"
    );
}

#[test]
fn a_script_is_downloaded_from_a_url() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = serve_script(Arc::clone(&counter));
    let script = PacScript::from_url(
        &format!("http://127.0.0.1:{port}/proxy.pac"),
        &PacSettings::default(),
    )
    .unwrap();
    assert_eq!(
        script.find_proxy_for_url("http://a/", "a").unwrap(),
        "PROXY a:8080"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[test]
fn a_resolver_downloads_a_stale_script_again_and_a_fresh_one_once() {
    let location = |port: u16| ProxySetting::Pac(format!("http://127.0.0.1:{port}/proxy.pac"));
    let build = |port: u16, ttl: Duration| {
        let mut settings = ResolverSettings::default();
        settings.pac.download_cache_ttl = ttl;
        Resolver::builder()
            .environment(Environment::from_pairs([("X", "y")]))
            .proxy(location(port))
            .resolver_settings(settings)
            .build()
    };

    let counter = Arc::new(AtomicUsize::new(0));
    let resolver = build(serve_script(Arc::clone(&counter)), Duration::from_secs(300));
    for _ in 0..3 {
        resolver.lookup("http://a.example/").unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    let counter = Arc::new(AtomicUsize::new(0));
    let resolver = build(serve_script(Arc::clone(&counter)), Duration::ZERO);
    for _ in 0..3 {
        resolver.lookup("http://a.example/").unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[test]
fn browser_compatibility_hides_the_path_of_a_secure_url() {
    let seen = |settings: &PacSettings| {
        let script = PacScript::builder(
            "function FindProxyForURL(url, host) { return 'PROXY ' + (url.indexOf('secret') >= 0 ? 'leaks' : 'hidden') + ':1'; }",
        )
        .settings(settings)
        .build()
        .unwrap();
        script
            .find_proxy_for_url("https://a.example/secret?x=1", "a.example")
            .unwrap()
    };
    assert_eq!(seen(&PacSettings::default()), "PROXY leaks:1");
    let mut browser = PacSettings::default();
    browser.browser_compatibility = true;
    assert_eq!(seen(&browser), "PROXY hidden:1");

    let source = PacSource::new(|url: &str, _host: &str| Ok(format!("PROXY {}:1", url.len())))
        .with_browser_compatibility(true);
    assert!(source.browser_compatibility());
    let Some(routes) = source.routes("https://a.example/secret").unwrap() else {
        panic!("an answer");
    };
    // `https://a.example:443/` is 22 bytes.
    assert!(matches!(&routes[0], Route::Proxy(proxy) if proxy.host() == "22"));
}

#[test]
fn a_pac_source_answers_over_any_find_proxy() {
    let script = PacScript::builder(SCRIPT).build().unwrap();
    let source = PacSource::new(script);
    let routes = source.routes("http://x.example/").unwrap().unwrap();
    assert!(matches!(&routes[0], Route::Proxy(proxy) if proxy.host() == "x.example"));
    let _ = source.script();
}

#[test]
fn wpad_finds_nothing_for_a_machine_name_without_a_domain() {
    let mut settings = PacSettings::default();
    settings.reader.dhcp_lease_paths.clear();
    settings.download_timeout = Duration::from_millis(200);
    assert!(discover(Some("pc1"), &settings).unwrap().is_none());
}
