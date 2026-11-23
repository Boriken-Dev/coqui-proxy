//! Every decoder of the sans-I/O cores takes arbitrary bytes and answers with a value or an
//! error, never a panic; a head over a limit is refused.

use coqui_proxy::ConnectorSettings;
use coqui_proxy::connect::core::{ConnectResponse, Progress, decode_chunk_size};
use coqui_proxy::connect::socks::{Greeting, PasswordLogin, Reply};
use proptest::prelude::*;

proptest! {
    #[test]
    fn no_decoder_panics_on_any_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        let settings = ConnectorSettings::default();
        let _ = ConnectResponse::decode(&bytes, &settings);
        let _ = decode_chunk_size(&bytes);
        let _ = Greeting::decode_choice(&bytes);
        let _ = PasswordLogin::decode_verdict(&bytes);
        let _ = Reply::decode4(&bytes);
        let _ = Reply::decode5(&bytes);
    }

    #[test]
    fn a_reply_prefix_is_incomplete_or_complete_never_beyond_its_length(
        status in 100u16..600,
        cut in 0usize..80,
    ) {
        let reply = format!("HTTP/1.1 {status} X\r\nA: b\r\n\r\n");
        let bytes = &reply.as_bytes()[..cut.min(reply.len())];
        match ConnectResponse::decode(bytes, &ConnectorSettings::default()).unwrap() {
            Progress::Complete { value, consumed } => {
                prop_assert_eq!(value.status(), status);
                prop_assert_eq!(consumed, reply.len());
            }
            Progress::Incomplete(more) => prop_assert!(more >= 1),
            _ => {}
        }
    }
}

#[test]
fn a_head_over_the_byte_limit_or_the_header_limit_is_refused() {
    let mut settings = ConnectorSettings::default();
    settings.max_response_head_bytes = 32;
    let long = format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "a".repeat(64));
    assert!(ConnectResponse::decode(long.as_bytes(), &settings).is_err());
    assert!(ConnectResponse::decode(&long.as_bytes()[..40], &settings).is_err());

    let mut settings = ConnectorSettings::default();
    settings.max_response_headers = 2;
    let many = "HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n";
    assert!(ConnectResponse::decode(many.as_bytes(), &settings).is_err());
    settings.max_response_headers = 3;
    assert!(ConnectResponse::decode(many.as_bytes(), &settings).is_ok());
}
