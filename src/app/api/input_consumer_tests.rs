use super::*;
use crate::pty::input_consumer::Principal;

fn request() -> CutRequest {
    CutRequest {
        epoch: "00112233445566778899aabbccddeeff".into(),
        seq: 1,
        token: "0f0e0d0c0b0a09080706050403020100".into(),
        cut: 2,
        digest: "d5bb8bd014e448a612372e955d5180bae3fb6d5fc3a1668e754a65c2abc341be".into(),
        kind: CutKind::Submit,
    }
}

fn enrollment() -> Enrollment<'static> {
    Enrollment {
        challenge: "0707070707070707070707070707070707070707070707070707070707070707",
        epoch: "00112233445566778899aabbccddeeff",
        nonce: "abababababababababababababababababababababababababababababababab",
        pane_id: "w1:p1",
        tty: (26, 3),
        peer_pid: 4242,
        pidns: 4026531836,
    }
}

fn paul() -> Principal {
    Principal {
        smarty_id: "paul".into(),
        display_name: "Paul".into(),
    }
}

fn cut_kinds() -> Vec<(&'static str, CutResult)> {
    vec![
        (
            "client_principal",
            CutResult::Client {
                principal: Some(paul()),
            },
        ),
        (
            "client_null_principal",
            CutResult::Client { principal: None },
        ),
        ("api", CutResult::Api),
        (
            "unknown_reason",
            CutResult::Unknown {
                reason: "seq_mismatch".into(),
            },
        ),
    ]
}

#[test]
fn input_consumer_v2_messages_use_the_v2_labels_and_field_lists() {
    let t = enroll_transcript(&enrollment());
    let e = enrollment();
    let expected = encode_fields(&[
        "herdr-enroll-v2",
        e.challenge,
        e.epoch,
        e.nonce,
        "w1:p1",
        "26",
        "3",
        "4242",
        "4026531836",
    ]);
    assert_eq!(t, expected);
    let m = cut_answer_message(
        &request(),
        &CutResult::Client {
            principal: Some(paul()),
        },
    );
    let r = request();
    assert_eq!(
        m,
        encode_fields(&[
            "herdr-cut-answer-v2",
            &r.epoch,
            "1",
            &r.token,
            "2",
            &r.digest,
            "submit",
            "client",
            "",
            "paul",
            "Paul",
        ])
    );
    // Domain separation: different label and field count.
    assert_ne!(&t[..19], &m[..19]);
}

#[test]
fn input_consumer_cut_answer_shape_has_sig_and_no_mac() {
    let sign = |_: &[u8]| Some(vec![7u8; 64]);
    for (_, result) in cut_kinds() {
        let answer = cut_answer(&request(), &result, sign).expect("signed");
        assert!(answer.get("mac").is_none());
        assert!(answer["sig"].is_string());
    }
    let client = cut_answer(&request(), &CutResult::Client { principal: None }, sign).unwrap();
    assert!(client["principal"].is_null() && client.get("principal").is_some());
    let api = cut_answer(&request(), &CutResult::Api, sign).unwrap();
    assert!(api.get("principal").is_none() && api.get("reason").is_none());
    let unknown = cut_answer(
        &request(),
        &CutResult::Unknown {
            reason: "seq_mismatch".into(),
        },
        sign,
    )
    .unwrap();
    assert_eq!(unknown["reason"], "seq_mismatch");
    // No signature means no answer, never an unsigned one.
    assert!(cut_answer(&request(), &CutResult::Api, |_| None).is_none());
    let enroll = enroll_answer(&enrollment(), sign).expect("signed");
    assert!(enroll.get("epoch_key").is_none());
    assert_eq!(
        enroll["peer"],
        serde_json::json!({"pid":"4242","pidns":"4026531836"})
    );
    assert_eq!(enroll["tty"], serde_json::json!({"dev":"26","ino":"3"}));
}

#[test]
fn input_consumer_enroll_without_key_is_refused_never_unsigned() {
    if crate::platform::server_key_available() {
        return;
    }
    let response = encode_consumer_response(
        "enroll".into(),
        ConsumerResponse::Enrolled {
            epoch: "e1".into(),
            nonce: "n".repeat(16),
            tty: (1, 2),
            peer_pid: 1,
            pidns: 2,
        },
        &AnswerContext::Enroll {
            pane_id: "w1:p1".into(),
            challenge: "07".repeat(32),
        },
    );
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(value["error"]["code"], "server_key_unavailable");
    assert!(value.get("result").is_none());
    let response = encode_consumer_response(
        "cut".into(),
        ConsumerResponse::Cut(CutResult::Api),
        &AnswerContext::Cut(request()),
    );
    let value: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(value["error"]["code"], "server_key_unavailable");
}

#[test]
fn input_consumer_release_and_refusal_envelopes() {
    let response = encode_consumer_response(
        "release".into(),
        ConsumerResponse::Released,
        &AnswerContext::Release,
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap(),
        serde_json::json!({"id":"release", "result":{}})
    );
    let response = encode_consumer_response(
        "refused".into(),
        ConsumerResponse::Refused {
            reason: "invalid_epoch".into(),
        },
        &AnswerContext::Cut(request()),
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"]["code"],
        "invalid_epoch"
    );
    assert!(is_lower_hex(&"ab".repeat(32), 64));
    assert!(!is_lower_hex(&"AB".repeat(32), 64));
    assert!(!is_lower_hex(&"ab".repeat(31), 64));
}
