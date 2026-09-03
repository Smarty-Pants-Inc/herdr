use std::io::Read;

use crate::api::schema::{
    EmptyParams, Method, Request, ServerLiveHandoffParams, ServerOmpMaintenanceAcquireParams,
    ServerOmpMaintenancePermitParams, ServerOmpMaintenanceReleaseParams,
};
use crate::server::omp_maintenance::OmpMaintenance;

pub(super) fn run_server_command(args: &[String]) -> std::io::Result<Option<i32>> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        return Ok(None);
    };

    match subcommand {
        "stop" => server_stop(&args[1..]).map(Some),
        "live-handoff" => server_live_handoff(&args[1..]).map(Some),
        "omp-maintenance" => server_omp_maintenance(&args[1..]).map(Some),
        "--handoff-import" => Ok(None),
        "reload-config" => server_reload_config(&args[1..]).map(Some),
        "agent-manifests" => server_agent_manifests(&args[1..]).map(Some),
        "update-agent-manifests" => server_update_agent_manifests(&args[1..]).map(Some),
        "reload-agent-manifests" => server_reload_agent_manifests(&args[1..]).map(Some),
        "help" | "--help" | "-h" => {
            print_server_help();
            Ok(Some(0))
        }
        _ => {
            print_server_help();
            Ok(Some(2))
        }
    }
}

fn server_stop(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server stop");
        return Ok(2);
    }

    match crate::session::stop_active_server() {
        Ok(()) => Ok(0),
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn server_reload_config(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server reload-config");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:server:reload-config".into(),
        method: Method::ServerReloadConfig(EmptyParams::default()),
    })?)
}

fn server_agent_manifests(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr server agent-manifests [--json]");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:server:agent-manifests".into(),
        method: Method::ServerAgentManifests(EmptyParams::default()),
    })?;
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    print_agent_manifest_status(&response);
    Ok(0)
}

fn server_reload_agent_manifests(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server reload-agent-manifests");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:server:reload-agent-manifests".into(),
        method: Method::ServerReloadAgentManifests(EmptyParams::default()),
    })?)
}

fn server_update_agent_manifests(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr server update-agent-manifests [--json]");
            return Ok(2);
        }
    };

    let response = match update_agent_manifest_status(super::send_request, || {
        crate::detect::manifest_update::check_and_update().map(|_| ())
    })? {
        Ok(response) => response,
        Err(err) => {
            if json {
                return super::print_response(&agent_manifest_update_error_response(&err));
            }
            eprintln!("failed to update agent detection manifests: {err}");
            return Ok(1);
        }
    };
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    print_agent_manifest_status(&response);
    Ok(0)
}

fn update_agent_manifest_status(
    mut send_request: impl FnMut(&Request) -> std::io::Result<serde_json::Value>,
    update_manifests: impl FnOnce() -> Result<(), String>,
) -> std::io::Result<Result<serde_json::Value, String>> {
    if let Err(err) = update_manifests() {
        return Ok(Err(err));
    }

    let reload_response = send_request(&Request {
        id: "cli:server:reload-agent-manifests".into(),
        method: Method::ServerReloadAgentManifests(EmptyParams::default()),
    })?;
    if reload_response.get("error").is_some() {
        return Ok(Ok(reload_response));
    }

    send_request(&Request {
        id: "cli:server:agent-manifests".into(),
        method: Method::ServerAgentManifests(EmptyParams::default()),
    })
    .map(Ok)
}

fn agent_manifest_update_error_response(err: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cli:server:update-agent-manifests",
        "error": {
            "code": "agent_manifest_update_failed",
            "message": err,
        }
    })
}

fn print_agent_manifest_status(response: &serde_json::Value) {
    let result = &response["result"];
    let last_check = result["last_check_unix"]
        .as_u64()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "never".to_string());
    let last_result = result["last_result"].as_str().unwrap_or("not checked");
    println!("last check: {last_check}");
    println!("result: {last_result}");
    println!();

    let Some(manifests) = result["manifests"].as_array() else {
        return;
    };
    for manifest in manifests {
        let agent = manifest["agent"].as_str().unwrap_or("-");
        let source = manifest["source_kind"].as_str().unwrap_or("-");
        let active_version = manifest["active_version"].as_str().unwrap_or("-");
        let remote_version = manifest["cached_remote_version"].as_str().unwrap_or("-");
        let remote_result = manifest["remote_update_result"]
            .as_str()
            .unwrap_or("not checked");
        let local_override_shadowing_remote = manifest["local_override_shadowing_remote"]
            .as_bool()
            .unwrap_or(false);
        let marker = if local_override_shadowing_remote {
            "!"
        } else if manifest["remote_update_error"].as_str().is_some() {
            "x"
        } else {
            " "
        };
        println!(
            "{marker} {agent:<9} {source:<14} active {active_version:<14} remote {remote_version:<14} {remote_result}"
        );
        if let Some(error) = manifest["remote_update_error"].as_str() {
            println!("  {error}");
        } else if local_override_shadowing_remote {
            println!("  local override shadows cached remote rules");
        } else if let Some(warning) = manifest["warning"].as_str() {
            println!("  {warning}");
        }
    }
}

fn server_omp_maintenance(args: &[String]) -> std::io::Result<i32> {
    let Some(action) = args.first().map(String::as_str) else {
        print_omp_maintenance_help();
        return Ok(2);
    };
    if matches!(action, "help" | "--help" | "-h") {
        print_omp_maintenance_help();
        return Ok(0);
    }
    let Some(options) = parse_omp_maintenance_options(&args[1..]) else {
        print_omp_maintenance_help();
        return Ok(2);
    };
    if action == "inspect"
        && !options.operation_id_stdin
        && options.proof_session.is_none()
        && options.proof_pane.is_none()
    {
        let response = match OmpMaintenance::inspect_host() {
            Ok(maintenance) => serde_json::json!({
                "id": "cli:server:omp-maintenance:inspect",
                "result": {"type": "omp_maintenance", "maintenance": maintenance},
            }),
            Err(error) => serde_json::json!({
                "id": "cli:server:omp-maintenance:inspect",
                "error": {"code": error.code(), "message": error.message()},
            }),
        };
        return super::print_response(&response);
    }

    let operation_id = || match read_omp_maintenance_capability_from_stdin() {
        Ok(operation_id) => Some(operation_id),
        Err(()) => {
            eprintln!("invalid OMP maintenance capability from stdin");
            None
        }
    };
    let method = match action {
        "acquire"
            if options.operation_id_stdin
                && options.proof_session.is_none()
                && options.proof_pane.is_none() =>
        {
            let Some(operation_id) = operation_id() else {
                return Ok(2);
            };
            Method::ServerOmpMaintenanceAcquire(ServerOmpMaintenanceAcquireParams { operation_id })
        }
        "status"
            if !options.operation_id_stdin
                && options.proof_session.is_none()
                && options.proof_pane.is_none() =>
        {
            Method::ServerOmpMaintenanceStatus(EmptyParams::default())
        }
        "inspect"
            if !options.operation_id_stdin
                && options.proof_session.is_none()
                && options.proof_pane.is_none() =>
        {
            Method::ServerOmpMaintenanceInspect(EmptyParams::default())
        }
        "permit"
            if options.operation_id_stdin
                && options.proof_session.is_some()
                && options.proof_pane.is_some() =>
        {
            let Some(operation_id) = operation_id() else {
                return Ok(2);
            };
            Method::ServerOmpMaintenancePermit(ServerOmpMaintenancePermitParams {
                operation_id,
                session: options.proof_session.unwrap_or_default(),
                pane_id: options.proof_pane.unwrap_or_default(),
            })
        }
        "release"
            if options.operation_id_stdin
                && options.proof_session.is_none()
                && options.proof_pane.is_none() =>
        {
            let Some(operation_id) = operation_id() else {
                return Ok(2);
            };
            Method::ServerOmpMaintenanceRelease(ServerOmpMaintenanceReleaseParams { operation_id })
        }
        _ => {
            print_omp_maintenance_help();
            return Ok(2);
        }
    };

    let mut request = Request {
        id: format!("cli:server:omp-maintenance:{action}"),
        method,
    };
    let response = super::send_request(&request);
    clear_omp_maintenance_capability_from_request(&mut request);
    super::print_response(&response?)
}

#[derive(Default)]
struct OmpMaintenanceOptions {
    operation_id_stdin: bool,
    proof_session: Option<String>,
    proof_pane: Option<String>,
}

fn parse_omp_maintenance_options(args: &[String]) -> Option<OmpMaintenanceOptions> {
    let mut options = OmpMaintenanceOptions::default();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--json" {
            idx += 1;
            continue;
        }
        if arg == "--operation-id"
            || arg.starts_with("--operation-id=")
            || arg.starts_with("--operation-id-stdin=")
        {
            return None;
        }
        if arg == "--operation-id-stdin" {
            if options.operation_id_stdin {
                return None;
            }
            options.operation_id_stdin = true;
            idx += 1;
            continue;
        }
        let (flag, value) = if let Some((flag, value)) = arg.split_once('=') {
            if value.is_empty() {
                return None;
            }
            (flag, value.to_string())
        } else {
            let value = args.get(idx + 1)?;
            if value.starts_with('-') {
                return None;
            }
            idx += 1;
            (arg.as_str(), value.clone())
        };
        let slot = match flag {
            "--proof-session" => &mut options.proof_session,
            "--proof-pane" => &mut options.proof_pane,
            _ => return None,
        };
        if slot.replace(value).is_some() {
            return None;
        }
        idx += 1;
    }
    Some(options)
}

const OMP_MAINTENANCE_CAPABILITY_BYTES: usize = 43;
const OMP_MAINTENANCE_CAPABILITY_STDIN_LIMIT: u64 = (OMP_MAINTENANCE_CAPABILITY_BYTES + 2) as u64;

fn read_omp_maintenance_capability_from_stdin() -> Result<String, ()> {
    read_omp_maintenance_capability(std::io::stdin().lock())
}

fn read_omp_maintenance_capability(mut input: impl Read) -> Result<String, ()> {
    let mut bytes = Vec::with_capacity(OMP_MAINTENANCE_CAPABILITY_BYTES + 1);
    let result = (|| {
        input
            .by_ref()
            .take(OMP_MAINTENANCE_CAPABILITY_STDIN_LIMIT)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        std::str::from_utf8(&bytes)
            .ok()
            .filter(|value| valid_omp_operation_id(value))
            .map(str::to_owned)
            .ok_or(())
    })();
    wipe_omp_maintenance_capability_bytes(&mut bytes);
    result
}

fn valid_omp_operation_id(value: &str) -> bool {
    const OPERATION_ID_BYTES: usize = 32;

    use base64::Engine as _;

    if value.len() != OMP_MAINTENANCE_CAPABILITY_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return false;
    }
    let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value) else {
        return false;
    };
    decoded.len() == OPERATION_ID_BYTES
        && base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(decoded) == value
}

fn clear_omp_maintenance_capability_from_request(request: &mut Request) {
    let operation_id = match &mut request.method {
        Method::ServerOmpMaintenanceAcquire(params) => &mut params.operation_id,
        Method::ServerOmpMaintenancePermit(params) => &mut params.operation_id,
        Method::ServerOmpMaintenanceRelease(params) => &mut params.operation_id,
        _ => return,
    };
    wipe_omp_maintenance_capability(operation_id);
}

fn wipe_omp_maintenance_capability(capability: &mut String) {
    // SAFETY: replacing UTF-8 bytes with NUL bytes preserves the String invariant.
    let bytes = unsafe { capability.as_mut_vec() };
    wipe_omp_maintenance_capability_bytes(bytes);
    capability.clear();
}

fn wipe_omp_maintenance_capability_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: each byte comes from a live, uniquely borrowed allocation.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

fn print_omp_maintenance_help() {
    eprintln!("usage: herdr server omp-maintenance acquire --operation-id-stdin [--json]");
    eprintln!("       herdr server omp-maintenance status [--json]");
    eprintln!("       herdr server omp-maintenance permit --operation-id-stdin --proof-session SESSION --proof-pane PANE [--json]");
    eprintln!("       herdr server omp-maintenance release --operation-id-stdin [--json]");
    eprintln!("       herdr server omp-maintenance inspect [--json]");
    eprintln!("capability stdin: exactly 43 canonical base64url bytes, optional LF, then EOF");
}

fn server_live_handoff(args: &[String]) -> std::io::Result<i32> {
    let Some(params) = parse_live_handoff_params(args) else {
        eprintln!(
            "usage: herdr server live-handoff [--import-exe <path>] [--expected-protocol <n>] [--expected-version <version>]"
        );
        return Ok(2);
    };

    // Live handoff is itself a protocol-mismatch recovery path, so it must
    // reach the running server without the normal CLI compatibility guard.
    let response = super::send_request_unchecked(&Request {
        id: "cli:server:live-handoff".into(),
        method: Method::ServerLiveHandoff(params),
    })?;
    if response.get("error").is_some() {
        let rendered = serde_json::to_string(&response).unwrap_or_else(|err| {
            format!(
                "{{\"error\":{{\"code\":\"render_failed\",\"message\":\"failed to render error response: {err}\"}}}}"
            )
        });
        eprintln!("{rendered}");
        return Ok(1);
    }

    eprintln!(
        "live handoff complete; server log: {}",
        crate::session::data_dir()
            .join("herdr-server.log")
            .display()
    );
    Ok(0)
}

fn parse_live_handoff_params(args: &[String]) -> Option<ServerLiveHandoffParams> {
    let mut params = ServerLiveHandoffParams::default();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        let (flag, value) = if let Some((flag, value)) = arg.split_once('=') {
            (flag, Some(value.to_string()))
        } else {
            let value = args.get(idx + 1).cloned();
            idx += 1;
            (arg.as_str(), value)
        };
        let value = value?;
        match flag {
            "--import-exe" => params.import_exe = Some(value),
            "--expected-protocol" => {
                params.expected_protocol = Some(value.parse().ok()?);
            }
            "--expected-version" => params.expected_version = Some(value),
            _ => return None,
        }
        idx += 1;
    }
    Some(params)
}

fn print_server_help() {
    eprintln!("herdr server commands:");
    eprintln!("  herdr server                run as headless server");
    eprintln!("  herdr server stop           stop the running server via the API socket");
    eprintln!("  herdr server live-handoff   hand off live panes to a new local server");
    eprintln!("  herdr server reload-config  reload config.toml in the running server");
    eprintln!("  herdr server omp-maintenance  control the host-wide OMP admission lease");
    eprintln!("  herdr server agent-manifests [--json]  show agent detection manifest status");
    eprintln!("  herdr server update-agent-manifests [--json]  fetch and reload agent detection manifests");
    eprintln!("  herdr server reload-agent-manifests  reload agent detection manifests in the running server");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_agent_manifest_status_fetches_reloads_then_reads_status() {
        let mut methods = Vec::new();
        let response = update_agent_manifest_status(
            |request| {
                methods.push(request.method.clone());
                match &request.method {
                    Method::ServerReloadAgentManifests(_) => Ok(serde_json::json!({
                        "id": request.id,
                        "result": { "type": "agent_manifest_reload", "manifests": [] }
                    })),
                    Method::ServerAgentManifests(_) => Ok(serde_json::json!({
                        "id": request.id,
                        "result": {
                            "type": "agent_manifest_status",
                            "last_result": "checked",
                            "manifests": []
                        }
                    })),
                    _ => panic!("unexpected request"),
                }
            },
            || Ok(()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(response["result"]["type"], "agent_manifest_status");
        assert_eq!(
            methods,
            vec![
                Method::ServerReloadAgentManifests(EmptyParams::default()),
                Method::ServerAgentManifests(EmptyParams::default())
            ]
        );
    }

    #[test]
    fn update_agent_manifest_status_skips_server_when_fetch_fails() {
        let response = update_agent_manifest_status(
            |_request| panic!("server should not be called after fetch failure"),
            || Err("network unavailable".to_string()),
        )
        .unwrap();

        assert_eq!(response, Err("network unavailable".to_string()));
        assert_eq!(
            agent_manifest_update_error_response("network unavailable")["error"]["code"],
            "agent_manifest_update_failed"
        );
    }

    #[test]
    fn update_agent_manifest_status_stops_after_reload_error() {
        let mut methods = Vec::new();
        let response = update_agent_manifest_status(
            |request| {
                methods.push(request.method.clone());
                Ok(serde_json::json!({
                    "id": request.id,
                    "error": {
                        "code": "reload_failed",
                        "message": "reload failed"
                    }
                }))
            },
            || Ok(()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(response["error"]["code"], "reload_failed");
        assert_eq!(
            methods,
            vec![Method::ServerReloadAgentManifests(EmptyParams::default())]
        );
    }

    #[test]
    fn omp_maintenance_options_require_one_private_stdin_source() {
        let args = vec![
            "--operation-id-stdin".to_string(),
            "--proof-session".to_string(),
            "proof".to_string(),
            "--proof-pane".to_string(),
            "w1:p1".to_string(),
            "--json".to_string(),
        ];
        let parsed = parse_omp_maintenance_options(&args).expect("options");
        assert!(parsed.operation_id_stdin);
        assert_eq!(parsed.proof_session.as_deref(), Some("proof"));
        assert_eq!(parsed.proof_pane.as_deref(), Some("w1:p1"));

        for args in [
            vec![
                "--operation-id-stdin".to_string(),
                "--operation-id-stdin".to_string(),
            ],
            vec!["--operation-id=legacy-capability".to_string()],
            vec![
                "--operation-id-stdin".to_string(),
                "--operation-id=legacy-capability".to_string(),
            ],
            vec!["--operation-id-stdin=unexpected".to_string()],
            vec!["--unknown=value".to_string()],
        ] {
            assert!(parse_omp_maintenance_options(&args).is_none());
        }
    }

    #[test]
    fn omp_maintenance_options_reject_option_flags_as_missing_values() {
        for flag in ["--proof-session", "--proof-pane"] {
            assert!(
                parse_omp_maintenance_options(&[flag.to_string(), "--json".to_string(),]).is_none()
            );
            assert!(parse_omp_maintenance_options(&[format!("{flag}=")]).is_none());
        }
    }

    #[test]
    fn omp_maintenance_capability_stdin_is_exact_and_preserves_leading_dash() {
        let capability = "-Pn6-_z9_v8AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhc";
        for input in [capability.to_string(), format!("{capability}\n")] {
            assert_eq!(
                read_omp_maintenance_capability(std::io::Cursor::new(input)),
                Ok(capability.to_string())
            );
        }

        for input in [
            "",
            "\n",
            "-Pn6-_z9_v8AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhc\n\n",
            "-Pn6-_z9_v8AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhc ",
            "-Pn6-_z9_v8AAQIDBAUGBwgJCgsMDQ4PEBESExQVFh\0",
            "-Pn6-_z9_v8AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcxx",
        ] {
            assert_eq!(
                read_omp_maintenance_capability(std::io::Cursor::new(input)),
                Err(())
            );
        }
    }

    #[test]
    fn live_handoff_params_parse_remote_update_fields() {
        let args = vec![
            "--import-exe".to_string(),
            "/home/me/.local/bin/herdr".to_string(),
            "--expected-protocol=9".to_string(),
            "--expected-version".to_string(),
            "0.6.2".to_string(),
        ];

        let params = parse_live_handoff_params(&args).expect("params");

        assert_eq!(
            params.import_exe.as_deref(),
            Some("/home/me/.local/bin/herdr")
        );
        assert_eq!(params.expected_protocol, Some(9));
        assert_eq!(params.expected_version.as_deref(), Some("0.6.2"));
    }
}
