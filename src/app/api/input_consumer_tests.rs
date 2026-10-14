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

/// Shared vectors V1-V3 (design "Acceptance tests"; H1), pinned verbatim from
/// dp2636/vectors-v2.json, which Pi's implementation generated. Ed25519 under the
/// fixed test seed 0x01..0x20 is deterministic, so Herdr must produce the exact
/// transcript bytes, digests, signatures and answer shapes.
#[cfg(target_os = "linux")]
mod vectors {
    use super::*;
    use base64::Engine as _;
    use sha2::Digest as _;

    const PINNED: &str = include_str!("../../../tests/fixtures/input-consumer-vectors-v2.json");

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }
    fn text(v: &serde_json::Value) -> &str {
        v.as_str().expect("string field")
    }
    fn number(v: &serde_json::Value) -> u64 {
        text(v).parse().expect("decimal field")
    }

    #[test]
    fn input_consumer_v2_shared_vectors_match_byte_for_byte() {
        use ring::signature::KeyPair as _;
        let pinned: serde_json::Value = serde_json::from_str(PINNED).expect("vectors json");
        let seed = unhex(text(&pinned["key"]["seed_hex"]));
        assert_eq!(seed, (1..=32).collect::<Vec<u8>>());
        let key = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).expect("seed");
        assert_eq!(
            hex(key.public_key().as_ref()),
            text(&pinned["key"]["public_key_hex"])
        );
        let sign = |m: &[u8]| Some(key.sign(m).as_ref().to_vec());

        let enroll = &pinned["enroll"];
        assert_eq!(text(&enroll["domain"]), ENROLL_LABEL);
        let f = &enroll["fields"];
        let enrollment = Enrollment {
            challenge: text(&f["challenge"]),
            epoch: text(&f["epoch"]),
            nonce: text(&f["nonce"]),
            pane_id: text(&f["pane_id"]),
            tty: (number(&f["tty"]["dev"]), number(&f["tty"]["ino"])),
            peer_pid: number(&f["peer"]["pid"]) as u32,
            pidns: number(&f["peer"]["pidns"]),
        };
        let t = enroll_transcript(&enrollment);
        assert_eq!(hex(&t), text(&enroll["message_hex"]));
        assert_eq!(
            hex(&sha2::Sha256::digest(&t)),
            text(&enroll["message_sha256"])
        );
        let answer = enroll_answer(&enrollment, sign).expect("signed");
        assert_eq!(answer["sig"], enroll["sig_b64"]);
        assert_eq!(answer["tty"], f["tty"]);
        assert_eq!(answer["peer"], f["peer"]);

        let cuts = pinned["cut"].as_array().expect("cut vectors");
        let mut names = Vec::new();
        for vector in cuts {
            assert_eq!(text(&vector["domain"]), CUT_ANSWER_LABEL);
            let r = &vector["request"];
            let request = CutRequest {
                epoch: text(&r["epoch"]).into(),
                seq: r["seq"].as_u64().expect("seq"),
                token: text(&r["token"]).into(),
                cut: r["cut"].as_u64().expect("cut"),
                digest: text(&r["digest"]).into(),
                kind: match text(&r["kind"]) {
                    "submit" => CutKind::Submit,
                    "discard" => CutKind::Discard,
                    other => panic!("kind {other}"),
                },
            };
            let a = &vector["answer"];
            let result = match text(&a["result"]) {
                "client" => CutResult::Client {
                    principal: serde_json::from_value(a["principal"].clone()).expect("principal"),
                },
                "api" => CutResult::Api,
                "mixed" => CutResult::Mixed,
                "unknown" => CutResult::Unknown {
                    reason: text(&a["reason"]).into(),
                },
                other => panic!("result {other}"),
            };
            let message = cut_answer_message(&request, &result);
            assert_eq!(hex(&message), text(&vector["message_hex"]), "{vector}");
            assert_eq!(
                hex(&sha2::Sha256::digest(&message)),
                text(&vector["message_sha256"])
            );
            let mut answer = cut_answer(&request, &result, sign).expect("signed");
            assert_eq!(answer["sig"], vector["sig_b64"], "{vector}");
            // The rest of the answer is exactly Pi's expected wire shape.
            answer.as_object_mut().expect("object").remove("sig");
            assert_eq!(&answer, a, "{vector}");
            names.push(text(&vector["name"]).to_owned());
        }
        assert_eq!(
            names,
            [
                "client-principal",
                "client-null-principal",
                "api",
                "unknown-reason"
            ]
        );
        // The pinned signatures are not merely reproduced: they verify.
        let verifier =
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key.public_key());
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::STANDARD
                .decode(text(v))
                .expect("base64")
        };
        verifier
            .verify(&t, &b64(&enroll["sig_b64"]))
            .expect("enroll vector verifies");
        for vector in cuts {
            verifier
                .verify(
                    &unhex(text(&vector["message_hex"])),
                    &b64(&vector["sig_b64"]),
                )
                .expect("cut vector verifies");
        }
    }
}
