#![cfg(all(windows, feature = "negotiate", feature = "tokio"))]

//! The Windows security package logs in to a stand-in proxy through the asynchronous connector.

mod support;

use coqui_proxy::{Error, Negotiate, Proxy, ProxyConnector, ProxyScheme, Route};
use support::{Require, StandInOrigin, StandInProxy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn route(proxy: &StandInProxy) -> Route {
    Route::Proxy(
        Proxy::builder(ProxyScheme::Http, "127.0.0.1", proxy.addr.port())
            .build()
            .unwrap(),
    )
}

#[tokio::test]
async fn a_proxy_the_policy_names_is_logged_in_to_with_the_logon_session() {
    let origin = StandInOrigin::start("negotiated");
    let proxy = StandInProxy::start(Require::Negotiate, origin.addr);
    let policy = Negotiate::builder().proxies(["127.0.0.1"]).build().unwrap();
    let connector = ProxyConnector::builder().negotiate(policy).build().asynch();

    let mut connection = connector
        .connect(&route(&proxy), "app.internal.test", 80)
        .await
        .unwrap();
    connection
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut reply = String::new();
    let _ = connection.read_to_string(&mut reply).await;
    assert!(reply.ends_with("negotiated"), "{reply}");

    let seen = proxy.seen();
    assert!(seen.len() >= 2, "{} requests", seen.len());
    assert!(seen[0].header("proxy-authorization").is_none());
    assert!(
        seen[1]
            .header("proxy-authorization")
            .is_some_and(|value| value.starts_with("Negotiate "))
    );
    assert_eq!(proxy.users().len(), 1);
}

#[tokio::test]
async fn a_proxy_the_policy_does_not_name_is_not_logged_in_to() {
    let origin = StandInOrigin::start("x");
    let proxy = StandInProxy::start(Require::Negotiate, origin.addr);
    let policy = Negotiate::builder()
        .proxies(["other.example"])
        .build()
        .unwrap();
    let connector = ProxyConnector::builder().negotiate(policy).build().asynch();
    let error = connector
        .connect(&route(&proxy), "app.internal.test", 80)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Login(_)), "{error}");
    assert!(proxy.users().is_empty());
}
