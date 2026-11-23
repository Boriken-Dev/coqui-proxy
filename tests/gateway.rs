//! URL-prefix gateways, `PREFIX` in a PAC result, and the errors that list unusable proxies.

use coqui_proxy::{Error, Proxy, ProxyConnector, ProxyScheme, Route, UnsupportedProxy};

#[test]
fn a_gateway_parses_with_its_path_and_builds_the_url_to_ask() {
    let gateway = Proxy::parse("prefix+https://gw.example/fetch/").unwrap();
    assert_eq!(gateway.scheme(), ProxyScheme::Gateway { tls: true });
    assert!(gateway.is_gateway());
    assert_eq!(gateway.path(), Some("/fetch/"));
    assert_eq!(gateway.port(), 443);
    assert_eq!(gateway.to_string(), "prefix+https://gw.example:443/fetch/");
    assert_eq!(
        gateway.gateway_url("https://example.com/a?b=1").as_deref(),
        Some("https://gw.example:443/fetch/https://example.com/a?b=1")
    );
    let plain = Proxy::parse("prefix+http://gw.example:8080").unwrap();
    assert_eq!(plain.path(), Some("/"));
    assert_eq!(plain.scheme().as_str(), "prefix+http");
}

#[test]
fn a_proxy_is_not_a_gateway_and_a_gateway_path_is_checked() {
    let proxy = Proxy::parse("http://proxy.example:3128").unwrap();
    assert!(!proxy.is_gateway() && proxy.path().is_none());
    assert_eq!(proxy.gateway_url("http://a/"), None);
    assert!(Proxy::parse("http://proxy.example:3128/pac").is_err());
    assert!(Proxy::parse("prefix+http://gw.example/a?b").is_err());
    let built = Proxy::builder(ProxyScheme::Gateway { tls: false }, "gw.example", 80)
        .path("/x/")
        .build()
        .unwrap();
    assert_eq!(built.path(), Some("/x/"));
    assert!(
        Proxy::builder(ProxyScheme::Http, "p.example", 80)
            .path("/x")
            .build()
            .is_err()
    );
}

#[test]
fn a_pac_prefix_entry_names_a_gateway_and_a_bad_one_is_dropped() {
    let routes = Route::parse_pac_result("PREFIX https://gw.example/fetch/; PROXY a.example:1");
    assert_eq!(routes.len(), 2);
    assert!(matches!(&routes[0], Route::Proxy(proxy) if proxy.is_gateway()));
    assert_eq!(
        Route::parse_pac_result("PREFIX ftp://nope"),
        [Route::Direct]
    );
}

#[test]
fn a_connector_does_not_tunnel_through_a_gateway() {
    let gateway = Proxy::parse("prefix+http://127.0.0.1:9/").unwrap();
    let error = ProxyConnector::default()
        .connect(&Route::Proxy(gateway), "example.com", 443)
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error}");
}

#[test]
fn the_unusable_proxies_are_listed_with_their_reasons_and_no_secrets() {
    let proxy = Proxy::parse("http://ada:s3cret@proxy.example:3128").unwrap();
    let listed = UnsupportedProxy::new("a client", vec![(proxy.clone(), "no CONNECT".to_string())]);
    assert_eq!(listed.client(), "a client");
    assert_eq!(listed.proxies(), [&proxy]);
    assert_eq!(listed.reasons(), ["no CONNECT"]);
    let error = Error::UnsupportedProxy(Box::new(listed));
    assert!(!error.to_string().contains("s3cret"), "{error}");
    assert_eq!(Error::MissingEngine.missing_feature(), Some("pac"));
    assert_eq!(Error::NoReachableProxy.missing_feature(), None);
    assert_eq!(coqui_proxy::VERSION, env!("CARGO_PKG_VERSION"));
}
