//! Golden roundtrip tests for the remote codec (PAR-CODEC, issue #31).
//!
//! Every generated v8 corpus row is decoded, checked against its message kind,
//! and re-encoded byte-for-byte. The corpus itself is generator-owned; this
//! test deliberately does not synthesize or rewrite fixture rows.

#![cfg(test)]
#![expect(
    clippy::expect_used,
    reason = "golden tests use expect for irrecoverable fixture and codec assertions"
)]
#![expect(
    clippy::panic,
    reason = "golden tests panic on committed fixture or protocol drift"
)]

use std::fs;

use serde::Deserialize;

use pi::remote::codec::{
    ClientMessageDecoder, CodecError, ServerMessageDecoder, create_client_message_decoder,
    create_server_message_decoder, decode_client_message, decode_server_message,
    encode_client_message, encode_server_message, is_supported_protocol_version,
};
use pi::remote::framing::{FrameDecoder, FrameError, assert_complete_frame, encode_frame};
use pi::remote::schemas::{
    ClientMessage, PROTOCOL_VERSION, RpcTarget, ServerMessage,
};
use pi_agent::service::value::JsonValue;

/// One row of the generator-owned golden corpus JSONL.
#[derive(Debug, Deserialize)]
struct CorpusRow {
    kind: String,
    #[serde(rename = "frameHex")]
    frame_hex: String,
    #[expect(
        dead_code,
        reason = "corpus schema field: present in JSONL for documentation, not read by tests"
    )]
    note: String,
}

#[expect(
    clippy::panic,
    reason = "test fixture: corpus file is committed; read failure is irrecoverable"
)]
#[expect(
    clippy::expect_used,
    reason = "test fixture: corpus rows are committed valid JSONL"
)]
fn load_corpus() -> Vec<CorpusRow> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/pi-remote-protocol/tests/fixtures/par-wire-corpus.jsonl");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read corpus at {}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("corpus row"))
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "test fixture: hex strings are committed valid hex"
)]
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("valid hex"))
        .collect()
}

fn assert_v8_kinds_present(corpus: &[CorpusRow]) {
    let expected = [
        "client_hello",
        "request",
        "cancel",
        "server_hello",
        "server_hello_error",
        "response_ok",
        "response_null",
        "response_absent",
        "response_error",
        "service_update",
        "attachment_null",
        "attachment_session",
        "over_limit_rejection",
    ];
    for kind in expected {
        assert!(
            corpus.iter().any(|row| row.kind == kind),
            "generator corpus is missing v8 row kind {kind}"
        );
    }
}

#[expect(
    clippy::panic,
    reason = "test assertion: a row with an unknown kind is corpus drift"
)]
#[expect(
    clippy::expect_used,
    reason = "test assertions: golden decode/encode must succeed"
)]
fn decode_and_reencode(row: &CorpusRow) -> Vec<u8> {
    let frame = hex_to_bytes(&row.frame_hex);
    match row.kind.as_str() {
        "client_hello" => {
            let message = decode_client_message(&frame, None).expect("decode client hello");
            assert!(matches!(
                &message,
                ClientMessage::Hello { version } if *version == PROTOCOL_VERSION
            ));
            encode_client_message(&message, None).expect("re-encode client hello")
        }
        "request" => {
            let message = decode_client_message(&frame, None).expect("decode request");
            assert!(matches!(
                &message,
                ClientMessage::Request {
                    target: RpcTarget::Session(_),
                    ..
                }
            ));
            encode_client_message(&message, None).expect("re-encode request")
        }
        "cancel" => {
            let message = decode_client_message(&frame, None).expect("decode cancel");
            assert!(matches!(
                &message,
                ClientMessage::Cancel {
                    target: RpcTarget::Server(_),
                    ..
                }
            ));
            encode_client_message(&message, None).expect("re-encode cancel")
        }
        "server_hello" => {
            let message = decode_server_message(&frame, None).expect("decode server hello");
            assert!(matches!(
                &message,
                ServerMessage::Hello { version, .. } if *version == PROTOCOL_VERSION
            ));
            encode_server_message(&message, None).expect("re-encode server hello")
        }
        "server_hello_error" => {
            let message = decode_server_message(&frame, None).expect("decode server hello error");
            assert!(matches!(&message, ServerMessage::HelloError { .. }));
            encode_server_message(&message, None).expect("re-encode server hello error")
        }
        "response_ok" => {
            let message = decode_server_message(&frame, None).expect("decode response ok");
            assert!(matches!(
                &message,
                ServerMessage::Response {
                    result: Some(result),
                    ..
                } if !result.is_null()
            ));
            encode_server_message(&message, None).expect("re-encode response ok")
        }
        "response_null" => {
            let message =
                decode_server_message(&frame, None).expect("decode explicit null response");
            assert!(matches!(
                &message,
                ServerMessage::Response {
                    result: Some(result),
                    ..
                } if result.is_null()
            ));
            encode_server_message(&message, None).expect("re-encode explicit null response")
        }
        "response_absent" => {
            let message = decode_server_message(&frame, None).expect("decode absent response");
            assert!(matches!(
                &message,
                ServerMessage::Response { result: None, .. }
            ));
            encode_server_message(&message, None).expect("re-encode absent response")
        }
        "response_error" => {
            let message = decode_server_message(&frame, None).expect("decode response error");
            assert!(matches!(&message, ServerMessage::ResponseError { .. }));
            encode_server_message(&message, None).expect("re-encode response error")
        }
        "service_update" => {
            let message = decode_server_message(&frame, None).expect("decode service update");
            assert!(matches!(&message, ServerMessage::ServiceUpdate { .. }));
            encode_server_message(&message, None).expect("re-encode service update")
        }
        "attachment_null" => {
            let message = decode_server_message(&frame, None).expect("decode detached attachment");
            assert!(matches!(
                &message,
                ServerMessage::Attachment { attachment: None }
            ));
            encode_server_message(&message, None).expect("re-encode detached attachment")
        }
        "attachment_session" => {
            let message = decode_server_message(&frame, None).expect("decode session attachment");
            assert!(matches!(
                &message,
                ServerMessage::Attachment {
                    attachment: Some(_)
                }
            ));
            encode_server_message(&message, None).expect("re-encode session attachment")
        }
        "over_limit_rejection" => panic!("over-limit row has no message to decode"),
        other => panic!("unknown v8 corpus row kind {other}"),
    }
}

// ---------------------------------------------------------------------------
// Byte-exact decode and encode of every generated v8 frame
// ---------------------------------------------------------------------------

#[test]
fn golden_v8_corpus_decodes_and_reencodes_byte_exact() {
    let corpus = load_corpus();
    assert_v8_kinds_present(&corpus);
    for row in corpus.iter().filter(|row| row.kind != "over_limit_rejection") {
        let frame = hex_to_bytes(&row.frame_hex);
        assert_eq!(
            decode_and_reencode(row),
            frame,
            "v8 corpus frame changed for row kind {}",
            row.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Over-limit rejection
// ---------------------------------------------------------------------------

#[test]
fn golden_over_limit_rejection() {
    let row = load_corpus()
        .into_iter()
        .find(|row| row.kind == "over_limit_rejection")
        .expect("over_limit_rejection row");
    let frame = hex_to_bytes(&row.frame_hex);
    let mut decoder = FrameDecoder::default();
    let error = decoder.push(&frame).expect_err("expected over-limit error");
    assert_eq!(
        error,
        FrameError::Oversized {
            declared: 16 * 1024 * 1024 + 1,
            limit: 16 * 1024 * 1024,
        }
    );
}

// ---------------------------------------------------------------------------
// Incremental decoder
// ---------------------------------------------------------------------------

#[test]
fn incremental_client_decoder_byte_by_byte() {
    let row = load_corpus()
        .into_iter()
        .find(|row| row.kind == "client_hello")
        .expect("client_hello row");
    let frame = hex_to_bytes(&row.frame_hex);
    let mut decoder = create_client_message_decoder(None).expect("create decoder");
    let mut messages = Vec::new();
    for byte in &frame {
        messages.extend(decoder.push(std::slice::from_ref(byte)).expect("push"));
    }
    decoder.end().expect("end");
    assert_eq!(messages.len(), 1);
    assert!(matches!(
        &messages[0],
        ClientMessage::Hello { version } if *version == PROTOCOL_VERSION
    ));
}

#[test]
fn incremental_server_decoder_multiple_v8_frames() {
    let corpus = load_corpus();
    let server_kinds = [
        "server_hello",
        "server_hello_error",
        "response_ok",
        "response_null",
        "response_absent",
        "response_error",
        "service_update",
        "attachment_null",
        "attachment_session",
    ];
    let mut combined = Vec::new();
    for kind in &server_kinds {
        let row = corpus
            .iter()
            .find(|row| row.kind == *kind)
            .expect("server row");
        combined.extend_from_slice(&hex_to_bytes(&row.frame_hex));
    }
    let mut decoder = create_server_message_decoder(None).expect("create decoder");
    let messages = decoder.push(&combined).expect("push");
    decoder.end().expect("end");
    assert_eq!(messages.len(), server_kinds.len());
}

// ---------------------------------------------------------------------------
// Typed error conditions
// ---------------------------------------------------------------------------

#[test]
fn truncated_frame_errors() {
    let row = load_corpus()
        .into_iter()
        .find(|row| row.kind == "client_hello")
        .expect("client_hello row");
    let frame = hex_to_bytes(&row.frame_hex);
    let error = decode_client_message(&frame[..frame.len() - 1], None).expect_err("expected error");
    assert!(matches!(
        error,
        CodecError::Frame(FrameError::NotOneCompletePayload)
    ));
}

#[test]
fn unknown_discriminant_errors() {
    // CBOR: {"type": "bogus", "version": 1}.
    let cbor: &[u8] = &[
        0xa2, // map(2)
        0x64, b't', b'y', b'p', b'e', // "type"
        0x65, b'b', b'o', b'g', b'u', b's', // "bogus"
        0x67, b'v', b'e', b'r', b's', b'i', b'o', b'n', // "version"
        0x01, // 1
    ];
    let error = decode_client_message(&encode_frame(cbor), None).expect_err("expected error");
    assert!(matches!(error, CodecError::UnknownDiscriminant(_)));
}

#[test]
fn server_hello_version_mismatch_errors() {
    let message = ServerMessage::Hello {
        version: PROTOCOL_VERSION + 1,
        server_id: pi::remote::schemas::ServerId::new(
            "00000000-0000-4000-8000-000000000001",
        )
        .expect("canonical server id"),
    };
    let error = encode_server_message(&message, None).expect_err("expected version mismatch");
    assert!(matches!(
        error,
        CodecError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            got: 9,
        }
    ));
}

#[test]
fn is_supported_protocol_version_works() {
    assert!(is_supported_protocol_version(PROTOCOL_VERSION));
    assert!(!is_supported_protocol_version(PROTOCOL_VERSION - 1));
    assert!(!is_supported_protocol_version(PROTOCOL_VERSION + 1));
}

// ---------------------------------------------------------------------------
// Absence witness: no R3/R4 symbols are part of the codec surface
// ---------------------------------------------------------------------------

#[test]
fn absence_witness_no_r3_r4_symbols() {
    let _ = PROTOCOL_VERSION;
    let _: fn(&[u8], Option<pi::remote::framing::FrameDecoderOptions>) -> Result<(), FrameError> =
        assert_complete_frame;
    let _: fn(&[u8]) -> Vec<u8> = encode_frame;
    let _: fn(u64) -> bool = is_supported_protocol_version;
    let _: fn(&ClientMessage, Option<_>) -> Result<Vec<u8>, CodecError> = encode_client_message;
    let _: fn(&ServerMessage, Option<_>) -> Result<Vec<u8>, CodecError> = encode_server_message;
    let _: fn(&[u8], Option<_>) -> Result<ClientMessage, CodecError> = decode_client_message;
    let _: fn(&[u8], Option<_>) -> Result<ServerMessage, CodecError> = decode_server_message;
    let _: fn(Option<_>) -> Result<ClientMessageDecoder, FrameError> =
        create_client_message_decoder;
    let _: fn(Option<_>) -> Result<ServerMessageDecoder, FrameError> =
        create_server_message_decoder;
}
