//! Origin-server login driven by hand against a loopback stand-in: the sans-I/O `OriginLogin`
//! says what to send, the test sends it on the same connection.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use coqui_proxy::{Credentials, OriginAuth};
use support::login_origin::{Demand, LoginOrigin};

fn basic(user: &str, password: &str) -> OriginAuth {
    OriginAuth::builder()
        .basic(["127.0.0.1"], Credentials::new(user, password))
        .build()
        .unwrap()
}

struct Reply {
    status: u16,
    challenges: Vec<String>,
    body: String,
}

fn read_reply(reader: &mut BufReader<TcpStream>) -> Reply {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let (mut challenges, mut length) = (Vec::new(), 0);
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        match name.to_ascii_lowercase().as_str() {
            "www-authenticate" => challenges.push(value.trim().to_string()),
            "content-length" => length = value.trim().parse().unwrap(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Reply {
        status,
        challenges,
        body: String::from_utf8(body).unwrap(),
    }
}

/// One GET, answering a `401` as often as the login allows, all on one connection.
fn get(origin: &LoginOrigin, host: &str, auth: &OriginAuth) -> Reply {
    let url = origin.url(host, "/ews");
    let mut reader = BufReader::new(TcpStream::connect(origin.addr).unwrap());
    let mut login = auth.login(&url).unwrap();
    let mut authorization = None::<String>;
    loop {
        let extra = authorization
            .take()
            .map(|value| format!("Authorization: {value}\r\n"))
            .unwrap_or_default();
        let request = format!("GET /ews HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n");
        reader.get_mut().write_all(request.as_bytes()).unwrap();
        let reply = read_reply(&mut reader);
        let Some(login) = login.as_mut().filter(|_| reply.status == 401) else {
            return reply;
        };
        let challenges: Vec<&str> = reply.challenges.iter().map(String::as_str).collect();
        match login.answer(&challenges).unwrap() {
            Some(answer) => authorization = Some(answer.authorization().to_string()),
            None => return reply,
        }
    }
}

#[test]
fn a_basic_challenge_is_answered_on_the_same_connection() {
    let origin = LoginOrigin::start(Demand::Basic("ada", "s3cret"));
    let reply = get(&origin, "127.0.0.1", &basic("ada", "s3cret"));
    assert_eq!((reply.status, reply.body.as_str()), (200, "welcome"));
    let log = origin.log();
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0].authorization, None);
    assert_eq!(
        log[1].authorization.as_deref(),
        Some("Basic YWRhOnMzY3JldA==")
    );
    assert_eq!(log[0].connection, log[1].connection);
}

#[test]
fn credentials_the_server_refuses_end_with_the_401_and_are_sent_once() {
    let origin = LoginOrigin::start(Demand::Basic("ada", "s3cret"));
    let reply = get(&origin, "127.0.0.1", &basic("ada", "wrong"));
    assert_eq!(reply.status, 401);
    assert_eq!(origin.log().len(), 2);
}

#[test]
fn a_server_without_a_rule_is_not_answered_and_gets_no_credentials() {
    let origin = LoginOrigin::start(Demand::Basic("ada", "s3cret"));
    let other = OriginAuth::builder()
        .basic(["mail.corp.example"], Credentials::new("ada", "s3cret"))
        .build()
        .unwrap();
    let reply = get(&origin, "127.0.0.1", &other);
    assert_eq!(reply.status, 401);
    let log = origin.log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].authorization, None);
}
