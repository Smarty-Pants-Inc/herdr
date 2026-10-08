//! #2636 RED real-server/slave-PTY regressions, §6 items 1–3 and 6–8.
//! Base contract: .local/spec-2636.md and .local/rulings-2636{,-r2}.md.
#![cfg(target_os = "linux")]
#[path = "support/input_consumer_cut.rs"]
mod cut_support;
pub mod support;
#[path = "support/command.rs"]
pub mod test_command;
use cut_support::{classification, client_text, request, unknown, Fixture};
use serde_json::json;
use std::thread;
use std::time::Duration;

#[test]
fn input_cut_joined_client_a_api_b_before_a_cut_then_client_b_fresh_c() {
    let mut f = Fixture::new();
    f.enroll();
    let mut c = f.client();
    client_text(&mut c, &f.pane, "A\r", false);
    f.wait_len(2);
    f.api_input("pane.send_text", json!({"text":"B"}));
    f.wait_len(3);
    let a = f.cut(json!({"cut":2}));
    assert_eq!(classification(&a), "client", "{a}");
    assert!(
        a["result"]["principal"].is_null(),
        "local client must be unmapped: {a}"
    );
    client_text(&mut c, &f.pane, "\r", false);
    f.wait_len(4);
    let b = f.cut(json!({"cut":4}));
    assert_eq!(classification(&b), "mixed", "{b}");
    client_text(&mut c, &f.pane, "C\r", false);
    f.wait_len(6);
    let c = f.cut(json!({"cut":6}));
    assert_eq!(classification(&c), "client", "{c}");
}
#[test]
fn input_cut_client_token_joins_private_durable_log_and_retry_is_once() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mut f = Fixture::new();
    let enrollment = f.enroll();
    let mut client = f.client();
    let input = "private-token-join-input\r";
    client_text(&mut client, &f.pane, input, false);
    assert_eq!(f.wait_len(input.len()), input.as_bytes());
    let token = "b63f127d944447329615b186a3abca28";
    let mut response = f.cut(json!({"token":token,"cut":input.len()}));
    // The child's _request is the actual RPC parameters, not a reconstructed
    // expected record. Keep the bearer capability out of assertion diagnostics.
    assert_eq!(classification(&response), "client");
    let params = response
        .as_object_mut()
        .unwrap()
        .remove("_request")
        .unwrap();
    assert_eq!(response["result"]["principal"], serde_json::Value::Null);
    assert_eq!(params["epoch"], enrollment["epoch"]);
    assert_eq!(params["token"], token);
    assert_eq!(params["seq"], 1);
    assert_eq!(params["cut"], input.len());
    assert_eq!(params["kind"], "submit");

    // Read immediately after the successful response: the real actor must have
    // published its audit record before releasing authoritative attribution.
    let (raw, records) = f.consumer_audit_records();
    assert_eq!(records.len(), 1, "exactly one committed cut record");
    let record = &records[0];
    assert_eq!(record.as_object().unwrap().len(), 7, "metadata-only record");
    for field in ["epoch", "seq", "token", "cut", "digest", "kind"] {
        assert_eq!(record[field], params[field], "audit join field {field}");
    }
    // The log keeps the answer Pi verified, without its per-epoch MAC.
    let mut answered = response["result"].clone();
    answered.as_object_mut().unwrap().remove("mac");
    assert_eq!(record["result"], answered);
    let metadata = std::fs::metadata(f.consumer_audit_path()).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
    // SAFETY: geteuid has no arguments, pointer access, or preconditions.
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    for forbidden in ["epoch_key", "nonce", "raw", "text", "input"] {
        assert!(
            !raw.contains(forbidden),
            "audit leaked forbidden field/content"
        );
    }
    for secret in [
        enrollment["epoch_key"].as_str().unwrap(),
        enrollment["nonce"].as_str().unwrap(),
        input,
        input.trim_end(),
    ] {
        assert!(
            !raw.contains(secret),
            "audit leaked capability, nonce or input"
        );
    }

    let retry = f.command(json!({"op":"rpc","method":"pane.input_consumer.cut","params":params}));
    assert_eq!(retry["result"], response["result"], "exact retry result");
    let (after_retry, retry_records) = f.consumer_audit_records();
    assert_eq!(
        retry_records.len(),
        1,
        "exact retry must not duplicate audit"
    );
    assert!(
        after_retry == raw,
        "exact retry must leave audit bytes unchanged"
    );
}

#[test]
fn input_cut_client_draft_api_only_enter_is_mixed() {
    let mut f = Fixture::new();
    f.enroll();
    let mut c = f.client();
    client_text(&mut c, &f.pane, "draft", false);
    f.wait_len(5);
    f.api_input("pane.send_keys", json!({"keys":["Enter"]}));
    f.wait_len(6);
    let r = f.cut(json!({}));
    assert_eq!(classification(&r), "mixed", "{r}");
}
#[test]
fn input_cut_client_api_paste_and_shift_enter_multiline() {
    let mut f = Fixture::new();
    f.enroll();
    let mut c = f.client();
    client_text(&mut c, &f.pane, "paste\nline\r", true);
    let first = f.wait_len(11).len();
    let r = f.cut(json!({"cut":first}));
    assert_eq!(classification(&r), "client", "{r}");
    f.api_input("pane.send_text", json!({"text":"api\npaste\r"}));
    f.wait_len(first + 10);
    let r = f.cut(json!({}));
    assert_eq!(classification(&r), "api", "{r}");
    let before = f.bytes().len();
    client_text(&mut c, &f.pane, "one", false);
    support::send_client_shell_shift_enter(&mut c, &f.pane).unwrap();
    client_text(&mut c, &f.pane, "two\r", false);
    f.wait_len(before + 8);
    let r = f.cut(json!({}));
    assert_eq!(classification(&r), "client", "one multiline interval: {r}");
}
#[test]
fn input_cut_second_same_group_and_tool_different_group_enroll_refused() {
    let mut f = Fixture::new();
    f.enroll();
    for different in [false, true] {
        let r = f.command(json!({"op":"child","different":different}));
        assert!(
            r["response"]["error"].is_object(),
            "second process different_group={different} must refuse: {r}"
        );
        if !different {
            assert_eq!(r["response"]["error"]["code"], "already_enrolled", "{r}");
        }
    }
}
fn nonraw_enroll(flag: &str) {
    let mut f = Fixture::new();
    let r = f.command(json!({"op":"enroll","flag":flag}));
    assert!(r["error"].is_object(), "{flag} enrollment must refuse: {r}");
    assert!(
        !r["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown variant"),
        "raw-mode refusal must not be missing method: {r}"
    );
}
fn changed_termios(flag: &str) {
    let mut f = Fixture::new();
    f.enroll();
    f.api_input("pane.send_text", json!({"text":"x\r"}));
    f.wait_len(2);
    f.command(json!({"op":"termios","flag":flag}));
    unknown(&f.cut(json!({})));
    unknown(&f.cut(json!({"kind":"discard"})));
}
fn content_echo(query: &str) {
    let mut f = Fixture::new();
    f.enroll();
    // A real CPR reply is an ordered output-processing fence. No sleep-only
    // inference of suppression and no fabricated emulator response bytes.
    let fenced = format!("{query}\x1b[6n");
    let hex = fenced
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    f.command(json!({"op":"query","hex":hex}));
    let cpr = regex::Regex::new(r"\x1b\[[0-9]+;[0-9]+R$").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let bytes = loop {
        let bytes = f.wait_len(1);
        if cpr.is_match(&String::from_utf8_lossy(&bytes)) {
            break bytes;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "real CPR fence missing: {bytes:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let text = String::from_utf8_lossy(&bytes);
    let fence = cpr.find(&text).unwrap();
    if query.starts_with("\x1bP$q") {
        assert!(
            fence.start() > 0,
            "DECRQSS must actually emit forbidden reply: {bytes:?}"
        );
        eprintln!("REAL_EMITTED_DECRQSS_WITH_CPR {bytes:?}");
        unknown(&f.cut(json!({})));
    } else {
        assert_eq!(
            fence.start(),
            0,
            "embedded engine should suppress OSC52/title/icon query: {bytes:?}"
        );
        eprintln!("REAL_SUPPRESSED_QUERY {query:?} CPR_FENCE={bytes:?}");
        unknown(&f.cut(json!({"kind":"discard"})));
        let before = bytes.len();
        let mut client = f.client();
        client_text(&mut client, &f.pane, "clean\r", false);
        f.wait_len(before + 6);
        let r = f.cut(json!({}));
        assert_eq!(
            classification(&r),
            "client",
            "suppressed query must not taint next client interval: {r}"
        );
    }
}
macro_rules! cases {
    ($helper:ident; $($name:ident => $value:expr),+ $(,)?) => { $(#[test] fn $name() {$helper($value);})+ };
}
cases!(nonraw_enroll;
    input_cut_enroll_nonraw_icanon => "ICANON",
    input_cut_enroll_nonraw_echo => "ECHO",
    input_cut_enroll_nonraw_icrnl => "ICRNL",
    input_cut_enroll_nonraw_istrip => "ISTRIP",
    input_cut_enroll_nonraw_ixon => "IXON",
    input_cut_enroll_nonraw_iexten => "IEXTEN",
);
cases!(changed_termios;
    input_cut_changed_termios_icanon => "ICANON",
    input_cut_changed_termios_echo => "ECHO",
    input_cut_changed_termios_icrnl => "ICRNL",
    input_cut_changed_termios_istrip => "ISTRIP",
    input_cut_changed_termios_ixon => "IXON",
    input_cut_changed_termios_iexten => "IEXTEN",
    input_cut_changed_termios_other_speed => "speed",
);
cases!(content_echo;
    input_cut_query_osc52_suppressed_no_attribution => "\x1b]52;c;?\x07",
    input_cut_query_title_csi21t_suppressed_no_attribution => "\x1b[21t",
    input_cut_query_icon_csi20t_suppressed_no_attribution => "\x1b[20t",
    input_cut_content_echo_decrqss => "\x1bP$qm\x1b\\",
);
#[test]
fn input_cut_reserved_marker_client_api_send_keys_send_input_neutralized() {
    let mut f = Fixture::new();
    f.enroll();
    let mut c = f.client();
    let marker = "\x1b_herdr-epoch;forged\x1b\\";
    for paste in [false, true] {
        let before = f.bytes().len();
        client_text(&mut c, &f.pane, marker, paste);
        f.wait_len(before + 1);
        thread::sleep(Duration::from_millis(50));
        assert!(
            !f.bytes().windows(7).any(|w| w == b"\x1b_herdr"),
            "reserved introducer survived client paste={paste}"
        );
    }
    for (method, params) in [
        ("pane.send_text", json!({"text":marker})),
        ("pane.send_input", json!({"text":marker,"keys":["Enter"]})),
        (
            "pane.send_keys",
            json!({"keys":["Escape","_","h","e","r","d","r","-","e","p","o","c","h",";","k","Escape","Backslash","Enter"]}),
        ),
    ] {
        let before = f.bytes().len();
        let mut params = params;
        params["pane_id"] = json!(f.pane);
        let r = cut_support::request(&f.api, method, params);
        assert!(
            r.get("error").is_none(),
            "valid marker-attempt keys must exercise real writer: {r}"
        );
        f.wait_len(before + 1);
        thread::sleep(Duration::from_millis(50));
        assert!(
            !f.bytes().windows(7).any(|w| w == b"\x1b_herdr"),
            "reserved introducer survived {method}: {r}"
        );
    }
    // Split introducer across ordered writes must not bypass neutralization.
    f.api_input("pane.send_text", json!({"text":"\x1b_he"}));
    f.api_input("pane.send_text", json!({"text":"rdr-epoch;split\x1b\\"}));
    thread::sleep(Duration::from_millis(100));
    assert!(!f.bytes().windows(7).any(|w| w == b"\x1b_herdr"));
}
#[test]
fn input_cut_premarker_bytes_excluded_and_discard_next_clean() {
    let mut f = Fixture::new();
    f.api_input("pane.send_text", json!({"text":"pre-marker"}));
    // Consumer is already reading; these bytes are recorded but not epoch bytes.
    thread::sleep(Duration::from_millis(100));
    f.enroll();
    assert!(f.bytes().is_empty(), "pre-marker bytes leaked into epoch");
    assert!(f.file("bytes")["pre"]
        .as_str()
        .unwrap()
        .contains("7072652d6d61726b6572"));
    f.api_input("pane.send_text", json!({"text":"discard\r"}));
    f.wait_len(8);
    f.cut(json!({"kind":"discard"}));
    let mut c = f.client();
    client_text(&mut c, &f.pane, "clean\r", false);
    f.wait_len(14);
    let r = f.cut(json!({}));
    assert_eq!(classification(&r), "client", "{r}");
}
#[test]
fn input_cut_exact_replay_conflict_new_seq_old_token_stale_seq() {
    let mut f = Fixture::new();
    f.enroll();
    f.api_input("pane.send_text", json!({"text":"a\r"}));
    f.wait_len(2);
    let r = f.cut(json!({}));
    let p = r["_request"].clone();
    let retry = f.command(json!({"op":"rpc","method":"pane.input_consumer.cut","params":p}));
    assert_eq!(
        retry["result"], r["result"],
        "exact retry must retain result"
    );
    for field in ["seq", "cut", "digest"] {
        let mut changed = p.clone();
        changed[field] = match field {
            "seq" => json!(2),
            "cut" => json!(1),
            _ => json!("00".repeat(32)),
        };
        let r = f.command(json!({"op":"rpc","method":"pane.input_consumer.cut","params":changed}));
        assert_eq!(r["error"]["code"], "token_conflict", "changed {field}: {r}");
    }
    let mut stale = p;
    stale["token"] = json!("f".repeat(32));
    let r = f.command(json!({"op":"rpc","method":"pane.input_consumer.cut","params":stale}));
    assert!(
        r.get("error").is_some() || classification(&r) == "unknown",
        "stale seq accepted: {r}"
    );
}
#[test]
fn input_cut_30_second_expiry_unknown() {
    let mut f = Fixture::new();
    f.enroll();
    f.api_input("pane.send_text", json!({"text":"old\r"}));
    f.wait_len(4);
    thread::sleep(Duration::from_secs(31));
    unknown(&f.cut(json!({})));
}
#[test]
fn input_cut_one_mib_raw_receipt_overflow_unknown() {
    let mut f = Fixture::new();
    f.enroll();
    // Bounded chunks avoid the JSON transport's line-size limit and keep the
    // slave draining while receipts accumulate on the real writer path.
    let chunk = "x".repeat(16384);
    for n in 1..=65 {
        f.api_input("pane.send_text", json!({"text":chunk}));
        f.wait_len(n * 16384);
    }
    unknown(&f.cut(json!({})));
}
#[test]
fn input_cut_desync_beyond_written_and_digest_mismatch_poison() {
    for bad in [json!({"cut":99}), json!({"digest":"00".repeat(32)})] {
        let mut f = Fixture::new();
        f.enroll();
        f.api_input("pane.send_text", json!({"text":"x\r"}));
        f.wait_len(2);
        unknown(&f.cut(bad));
        unknown(&f.cut(json!({"kind":"discard"})));
    }
}
#[test]
fn input_cut_second_slave_reader_stolen_byte_digest_mismatch_poison() {
    let mut f = Fixture::new();
    f.enroll();
    f.start(json!({"op":"steal"}));
    f.file("steal-ready");
    f.api_input("pane.send_text", json!({"text":"z"}));
    let stolen = f.finish();
    assert_eq!(
        stolen["bytes"], "7a",
        "second reader must actually steal byte"
    );
    f.api_input("pane.send_text", json!({"text":"a\r"}));
    f.wait_len(2);
    // The primary consumer saw only a\r; its digest omits the stolen z.
    unknown(&f.cut(json!({"cut":3})));
    unknown(&f.cut(json!({"kind":"discard"})));
}
#[test]
fn input_cut_unmapped_one_client_principal_null_two_connections_mixed() {
    let mut f = Fixture::new();
    f.enroll();
    let mut a = f.client();
    let mut b = f.client();
    client_text(&mut a, &f.pane, "local\r", false);
    f.wait_len(6);
    let r = f.cut(json!({}));
    assert_eq!(classification(&r), "client", "{r}");
    assert!(
        r["result"].get("principal").is_some_and(|p| p.is_null()),
        "unmapped result requires explicit principal null: {r}"
    );
    client_text(&mut a, &f.pane, "a", false);
    f.wait_len(7);
    client_text(&mut b, &f.pane, "b\r", false);
    f.wait_len(9);
    let r = f.cut(json!({}));
    assert_eq!(classification(&r), "mixed", "{r}");
}

#[test]
fn input_cut_real_client_api_marker_injection_before_any_enroll() {
    let f = Fixture::new();
    let mut c = f.client();
    // Prove a real client connection reaches this raw slave, independently of
    // unavailable enrollment. Shells must never get an implicit epoch marker.
    client_text(&mut c, &f.pane, "client-probe", false);
    f.api_input("pane.send_text", json!({"text":"api-probe"}));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let bytes = f.file("bytes");
        let raw = bytes["raw"].as_str().unwrap();
        if raw.contains("636c69656e742d70726f6265") && raw.contains("6170692d70726f6265") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "real client/API delivery: {bytes}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    eprintln!("REAL_CLIENT_API_BYTES {}", f.file("bytes"));
    client_text(&mut c, &f.pane, "\x1b_herdr-epoch;injection\x1b\\", false);
    thread::sleep(Duration::from_millis(150));
    let bytes = f.file("bytes");
    assert!(
        !bytes["raw"].as_str().unwrap().contains("1b5f6865726472"),
        "reserved introducer must be neutralized even before enroll: {bytes}"
    );
}

#[test]
fn input_cut_host_appearance_reply_contains_no_reserved_introducer() {
    let mut f = Fixture::new();
    f.enroll();
    let mut c = f.client();
    // The public appearance wire has only Dark/Light (no text field). Drive
    // the actual appearance writer after a subscription from the real pane.
    // There is no raw-string injection seam in this numeric/enum-only codec.
    f.command(json!({"op":"query","hex":"1b5b3f3230333168"}));
    let payload = [17u8, 2, 0]; // ClientShellHostTheme / Appearance / Dark
    use std::io::Write;
    c.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    c.write_all(&payload).unwrap();
    let payload = [17u8, 2, 1]; // Appearance / Light
    c.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    c.write_all(&payload).unwrap();
    c.flush().unwrap();
    f.wait_len(1);
    assert!(!f.bytes().windows(7).any(|w| w == b"\x1b_herdr"));
    // Appearance reports are not on the explicit neutral reply allow-list.
    unknown(&f.cut(json!({})));
}

#[test]
fn input_cut_linux_enroll_without_server_auth_works_unsigned() {
    // smarty-dev#6690 option 1: no server authentication in this build. Linux still
    // advertises and enrolls; the answer has no `sig`, so Pi labels turns `terminal`.
    let mut f = Fixture::new();
    let ping = request(&f.api, "ping", json!({}));
    assert_eq!(
        ping["result"]["capabilities"]["input_consumer"], true,
        "{ping}"
    );
    let epoch = f.enroll();
    assert!(epoch.get("sig").is_none(), "{epoch}");
    // The unsigned epoch still cuts with a MAC that verifies under its key.
    f.api_input("pane.send_text", json!({"text":"x\r"}));
    f.wait_len(2);
    let r = f.cut(json!({}));
    assert_eq!(classification(&r), "api", "{r}");
}
#[test]
fn input_cut_enroll_rejects_malformed_challenge() {
    let mut f = Fixture::new();
    for challenge in ["", "AB", &"AB".repeat(32), &"0".repeat(63)] {
        let r = f.command(json!({"op":"enroll","challenge":challenge}));
        assert_eq!(r["error"]["code"], "invalid_params", "{challenge:?}: {r}");
    }
    // The counterpart: a well-formed challenge still enrolls.
    f.enroll();
}
