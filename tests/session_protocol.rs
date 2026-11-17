//! Protocol seam: the versioned, newline-delimited wire format used by the
//! control client and the session owner. These tests are pure encode/decode
//! checks — no sockets or processes involved.

use murmduck::session::SessionState;
use murmduck::session::protocol::{self, Command, PROTOCOL_VERSION, Response};

#[test]
fn every_command_round_trips_through_the_wire_format() {
    for command in [Command::Stop, Command::ReplaceSelection, Command::Status] {
        let line = protocol::encode_command(command);
        assert!(line.ends_with('\n'), "a command must be newline-delimited");

        let decoded = protocol::decode_command(line.as_bytes()).expect("decodes");
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.command, command);
    }
}

#[test]
fn a_response_round_trips_with_its_session_state() {
    let line = protocol::encode_response(Response::Ok {
        state: SessionState::Playing,
    });
    assert!(line.ends_with('\n'));

    let decoded = protocol::decode_response(line.as_bytes()).expect("decodes");
    assert_eq!(decoded.version, PROTOCOL_VERSION);
    assert_eq!(
        decoded.response,
        Response::Ok {
            state: SessionState::Playing
        }
    );
}

#[test]
fn a_wrong_protocol_version_is_rejected() {
    // A future/foreign peer must not be mistaken for a compatible one.
    let line = format!(
        "{{\"version\":{},\"command\":\"Stop\"}}\n",
        PROTOCOL_VERSION + 1
    );
    let err = protocol::decode_command(line.as_bytes()).expect_err("version rejected");
    assert!(
        format!("{err}").contains("version"),
        "error should name the version problem: {err}"
    );
}

#[test]
fn malformed_and_empty_frames_are_rejected() {
    assert!(protocol::decode_command(b"not json\n").is_err());
    assert!(protocol::decode_command(b"").is_err());
    assert!(protocol::decode_response(b"{\"version\":1}\n").is_err());
}
