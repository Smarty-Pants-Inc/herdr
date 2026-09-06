use std::ffi::{OsStr, OsString};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;

pub(crate) fn command_for_argv_in_dir(program: &str, args: &[String], cwd: &Path) -> Command {
    let program = program_for_cwd(program, cwd);
    let mut command = command_for_program(&program);
    command.args(args).current_dir(cwd);
    command
}

pub(crate) fn pty_command_for_argv_in_dir(
    argv: &[String],
    cwd: &Path,
) -> std::io::Result<portable_pty::CommandBuilder> {
    let Some((program, args)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "execution provider command must not be empty",
        ));
    };
    let program = program_for_cwd(program, cwd);
    let mut command = portable_pty::CommandBuilder::new(program);
    command.args(args);
    command.cwd(cwd);
    Ok(command)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionProviderCommandKind {
    Pty,
    Process,
}
#[cfg(unix)]
const EXECUTION_PROVIDER_INHERITED_ENV_REMOVALS: &[&str] = &[
    "CODEX_THREAD_ID",
    "TMUX",
    "STY",
    "ZELLIJ",
    crate::integration::HERDR_WORKSPACE_ID_ENV_VAR,
    crate::integration::HERDR_TAB_ID_ENV_VAR,
    crate::integration::HERDR_PANE_ID_ENV_VAR,
    "HERDR_VIEW_ID",
    "HERDR_BUILD_BIN_PATH",
    "HERDR_OMP_BRIDGE",
    "HERDR_OMP_BRIDGE_TOKEN",
    "HERDR_OMP_GUEST_BRIDGE_TOKEN",
];
#[cfg(not(unix))]
const EXECUTION_PROVIDER_INHERITED_ENV_REMOVALS: &[&str] = &[];

pub(crate) fn execution_provider_inherited_env_removals() -> Vec<String> {
    EXECUTION_PROVIDER_INHERITED_ENV_REMOVALS
        .iter()
        .map(|key| (*key).to_string())
        .collect()
}

#[cfg(all(test, unix))]
mod inherited_env_tests {
    #[test]
    fn execution_provider_removals_include_omp_bridge_capabilities() {
        let removals = super::execution_provider_inherited_env_removals();
        for key in crate::integration::HERDR_OMP_BRIDGE_ENV_VARS {
            assert!(removals.iter().any(|removed| removed == key));
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedExecutionProvider {
    pub plugin_id: String,
    pub command: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub env: Vec<(String, String)>,
    pub remove_env: Vec<String>,
}

pub(crate) fn resolve_installed_execution_provider(
    scheme: &str,
    kind: ExecutionProviderCommandKind,
) -> std::io::Result<ResolvedExecutionProvider> {
    #[cfg(not(unix))]
    {
        let _ = (scheme, kind);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "execution-provider targets require a Unix Herdr server",
        ))
    }

    #[cfg(unix)]
    {
        let entries = crate::persist::plugin_registry::try_load()?;
        let mut matches = Vec::new();
        let mut disabled = Vec::new();
        let mut incompatible = Vec::new();
        let mut unsupported = Vec::new();
        let mut unavailable = Vec::new();

        for stored in entries {
            let cached_match = stored
                .execution_providers
                .iter()
                .any(|provider| provider.scheme == scheme);
            let plugin =
                match crate::app::load_plugin_manifest(&stored.manifest_path, stored.enabled) {
                    Ok(plugin) => plugin,
                    Err((_, message)) => {
                        if cached_match {
                            unavailable.push(format!("{}: {message}", stored.plugin_id));
                        }
                        continue;
                    }
                };
            if plugin.plugin_id != stored.plugin_id {
                if cached_match
                    || plugin
                        .execution_providers
                        .iter()
                        .any(|provider| provider.scheme == scheme)
                {
                    unavailable.push(format!(
                        "{}: reloaded manifest id {} does not match the registry",
                        stored.plugin_id, plugin.plugin_id
                    ));
                }
                continue;
            }
            let Some(provider) = plugin
                .execution_providers
                .iter()
                .find(|provider| provider.scheme == scheme)
            else {
                continue;
            };
            if !plugin.enabled {
                disabled.push(plugin.plugin_id.clone());
                continue;
            }
            if provider.protocol != crate::execution::EXECUTION_PROVIDER_PROTOCOL {
                incompatible.push(format!(
                    "{} declares protocol {}",
                    plugin.plugin_id, provider.protocol
                ));
                continue;
            }
            if let Err((_, message)) = crate::app::ensure_platform_supported(
                crate::app::effective_platforms(&provider.platforms, &plugin.platforms),
                &format!(
                    "execution provider {} from plugin {}",
                    scheme, plugin.plugin_id
                ),
            ) {
                unsupported.push(message);
                continue;
            }
            let mut env = crate::app::plugin_path_env(&plugin);
            env.push(("HERDR_ENV".into(), "1".into()));
            env.push(("HERDR_PLUGIN_ID".into(), plugin.plugin_id.clone()));
            if let Ok(current_exe) = std::env::current_exe() {
                env.push(("HERDR_BIN_PATH".into(), current_exe.display().to_string()));
            }
            let plugin_id = plugin.plugin_id.clone();
            matches.push((
                plugin_id.clone(),
                ResolvedExecutionProvider {
                    plugin_id,
                    command: match kind {
                        ExecutionProviderCommandKind::Pty => provider.pty_command.clone(),
                        ExecutionProviderCommandKind::Process => provider.process_command.clone(),
                    },
                    cwd: std::path::PathBuf::from(plugin.plugin_root),
                    env,
                    remove_env: EXECUTION_PROVIDER_INHERITED_ENV_REMOVALS
                        .iter()
                        .map(|key| (*key).to_string())
                        .collect(),
                },
            ));
        }

        match matches.len() {
            1 => Ok(matches.remove(0).1),
            count if count > 1 => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "execution provider scheme {scheme} is ambiguous across enabled plugins: {}",
                    matches
                        .iter()
                        .map(|(plugin_id, _)| plugin_id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
            _ if !unavailable.is_empty() => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "execution provider scheme {scheme} is unavailable: {}",
                    unavailable.join("; ")
                ),
            )),
            _ if !incompatible.is_empty() => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "execution provider scheme {scheme} has no compatible protocol {} provider: {}",
                    crate::execution::EXECUTION_PROVIDER_PROTOCOL,
                    incompatible.join("; ")
                ),
            )),
            _ if !unsupported.is_empty() => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!(
                    "execution provider scheme {scheme} is unsupported on this platform: {}",
                    unsupported.join("; ")
                ),
            )),
            _ if !disabled.is_empty() => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "execution provider scheme {scheme} is disabled in plugin{} {}",
                    if disabled.len() == 1 { "" } else { "s" },
                    disabled.join(", ")
                ),
            )),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no installed execution provider declares scheme {scheme}"),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum PluginCommandTarget {
    Pane {
        entrypoint: String,
    },
    Action {
        action_id: String,
        link_handler_id: Option<String>,
    },
}

#[cfg(windows)]
const _: () = {
    let _ = crate::app::effective_platforms;
    let _ = crate::app::ensure_platform_supported;
    let _ = crate::app::plugin_path_env;
};

#[cfg(unix)]
pub(crate) struct ResolvedPluginCommand {
    pub command: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub env: Vec<(String, String)>,
}

#[cfg(unix)]
pub(crate) fn resolve_installed_plugin_command(
    plugin_id: &str,
    target: &PluginCommandTarget,
) -> std::io::Result<ResolvedPluginCommand> {
    let stored = crate::persist::plugin_registry::try_load()?
        .into_iter()
        .find(|plugin| plugin.plugin_id == plugin_id)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("plugin {plugin_id} is not installed"),
            )
        })?;
    let plugin = crate::app::load_plugin_manifest(&stored.manifest_path, stored.enabled)
        .map_err(|(_, message)| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;
    if plugin.plugin_id != plugin_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "reloaded plugin manifest id {} does not match requested registry id {plugin_id}",
                plugin.plugin_id
            ),
        ));
    }
    if !plugin.enabled {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("plugin {plugin_id} is disabled"),
        ));
    }

    let (command, subject, target_env) = match target {
        PluginCommandTarget::Pane { entrypoint } => {
            let pane = plugin
                .panes
                .iter()
                .find(|pane| pane.id == *entrypoint)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("plugin pane {plugin_id}.{entrypoint} was not found"),
                    )
                })?;
            let subject = format!("plugin pane {plugin_id}.{entrypoint}");
            crate::app::ensure_platform_supported(
                crate::app::effective_platforms(&pane.platforms, &plugin.platforms),
                &subject,
            )
            .map_err(|(_, message)| {
                std::io::Error::new(std::io::ErrorKind::Unsupported, message)
            })?;
            (
                pane.command.clone(),
                subject,
                Some(("HERDR_PLUGIN_ENTRYPOINT_ID", entrypoint.as_str())),
            )
        }
        PluginCommandTarget::Action {
            action_id,
            link_handler_id,
        } => {
            let action = plugin
                .actions
                .iter()
                .find(|action| action.id == *action_id)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("plugin action {plugin_id}.{action_id} was not found"),
                    )
                })?;
            if let Some(link_handler_id) = link_handler_id {
                let handler = plugin
                    .link_handlers
                    .iter()
                    .find(|handler| handler.id == *link_handler_id)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!(
                                "plugin link handler {plugin_id}.{link_handler_id} was not found"
                            ),
                        )
                    })?;
                if handler.action != *action_id {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "plugin link handler {plugin_id}.{link_handler_id} does not invoke action {action_id}"
                        ),
                    ));
                }
                crate::app::ensure_platform_supported(
                    crate::app::effective_platforms(&handler.platforms, &plugin.platforms),
                    &format!("plugin link handler {plugin_id}.{link_handler_id}"),
                )
                .map_err(|(_, message)| {
                    std::io::Error::new(std::io::ErrorKind::Unsupported, message)
                })?;
            }
            let subject = format!("plugin action {plugin_id}.{action_id}");
            crate::app::ensure_platform_supported(
                crate::app::effective_platforms(&action.platforms, &plugin.platforms),
                &subject,
            )
            .map_err(|(_, message)| {
                std::io::Error::new(std::io::ErrorKind::Unsupported, message)
            })?;
            (
                action.command.clone(),
                subject,
                Some(("HERDR_PLUGIN_ACTION_ID", action_id.as_str())),
            )
        }
    };
    crate::app::ensure_plugin_user_dirs(&plugin)?;
    if command.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{subject} has an empty command"),
        ));
    }

    let mut env = crate::app::plugin_path_env(&plugin);
    env.push(("HERDR_PLUGIN_ID".into(), plugin.plugin_id.clone()));
    if let Some((key, value)) = target_env {
        env.push((key.into(), value.into()));
    }
    Ok(ResolvedPluginCommand {
        command,
        cwd: std::path::PathBuf::from(plugin.plugin_root),
        env,
    })
}

fn program_for_cwd(program: &str, cwd: &Path) -> OsString {
    let path = Path::new(program);
    let has_separator = program.contains('/') || (cfg!(windows) && program.contains('\\'));
    if path.is_relative() && has_separator {
        let relative = path.strip_prefix(Path::new(".")).unwrap_or(path);
        cwd.join(relative).into_os_string()
    } else {
        path.as_os_str().to_os_string()
    }
}

#[cfg(not(windows))]
fn command_for_program(program: &OsStr) -> Command {
    crate::noninteractive_process::command(program)
}

#[cfg(windows)]
fn command_for_program(program: &OsStr) -> Command {
    let resolved = resolve_windows_program(program);
    let command_program = resolved.as_ref().map_or_else(
        || program.to_os_string(),
        |path| path.as_os_str().to_os_string(),
    );
    if is_windows_batch_file_name(program)
        || resolved
            .as_ref()
            .is_some_and(|path| is_windows_batch_path(path))
    {
        let shell =
            std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into());
        let mut command = crate::noninteractive_process::command(shell);
        command.arg("/d").arg("/c").arg(command_program);
        command
    } else {
        crate::noninteractive_process::command(command_program)
    }
}

#[cfg(windows)]
fn resolve_windows_program(program: &OsStr) -> Option<PathBuf> {
    if has_path_separator(program) {
        return None;
    }
    let path = Path::new(program);
    if path.extension().is_some() {
        return std::env::var_os("PATH").and_then(|path_var| {
            std::env::split_paths(&path_var)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        });
    }
    let extensions = windows_path_extensions();
    std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var).find_map(|dir| {
            extensions
                .iter()
                .map(|extension| {
                    let mut file_name = program.to_os_string();
                    file_name.push(extension);
                    dir.join(file_name)
                })
                .find(|candidate| candidate.is_file())
        })
    })
}

#[cfg(windows)]
fn windows_path_extensions() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| {
                    if part.starts_with('.') {
                        part.to_string()
                    } else {
                        format!(".{part}")
                    }
                })
                .collect::<Vec<_>>()
        })
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or_else(|| {
            vec![
                ".COM".to_string(),
                ".EXE".to_string(),
                ".BAT".to_string(),
                ".CMD".to_string(),
            ]
        })
}

#[cfg(windows)]
fn has_path_separator(program: &OsStr) -> bool {
    program.to_string_lossy().contains(['/', '\\'])
}

#[cfg(windows)]
fn is_windows_batch_path(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(is_windows_batch_extension)
}

#[cfg(any(windows, test))]
fn is_windows_batch_file_name(program: &OsStr) -> bool {
    Path::new(program)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(is_windows_batch_extension)
}

#[cfg(any(windows, test))]
fn is_windows_batch_extension(extension: &str) -> bool {
    extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_explicit_relative_program_against_working_directory() {
        let cwd = Path::new("plugin-root");

        assert_eq!(
            program_for_cwd("./bin/tool", cwd),
            cwd.join("bin/tool").into_os_string()
        );
        assert_eq!(program_for_cwd("tool", cwd), OsString::from("tool"));
    }

    #[test]
    fn recognizes_windows_batch_extensions_case_insensitively() {
        assert!(is_windows_batch_file_name(OsStr::new("npm.cmd")));
        assert!(is_windows_batch_file_name(OsStr::new("script.BAT")));
        assert!(!is_windows_batch_file_name(OsStr::new("node.exe")));
        assert!(!is_windows_batch_file_name(OsStr::new("node")));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_reloaded_manifest_with_mismatched_plugin_id() {
        let root = std::env::temp_dir().join(format!(
            "herdr-plugin-command-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create plugin fixture");
        let manifest_path = root.join("herdr-plugin.toml");
        let manifest = |id: &str| {
            format!(
                r#"
id = "{id}"
name = "Test Plugin"
version = "0.1.0"
min_herdr_version = "{}"

[[actions]]
id = "run"
title = "Run"
command = ["true"]
"#,
                crate::build_info::BASE_VERSION
            )
        };
        std::fs::write(&manifest_path, manifest("example.requested"))
            .expect("write requested manifest");
        let stored = crate::app::load_plugin_manifest(&manifest_path.display().to_string(), true)
            .expect("load requested manifest");
        let registry_path = root.join("plugins.json");
        crate::persist::plugin_registry::save_to_path(&registry_path, &[stored])
            .expect("save plugin registry");
        std::fs::write(&manifest_path, manifest("example.reloaded"))
            .expect("replace manifest with mismatched id");

        let result =
            crate::persist::plugin_registry::with_test_registry_path(registry_path, || {
                resolve_installed_plugin_command(
                    "example.requested",
                    &PluginCommandTarget::Action {
                        action_id: "run".into(),
                        link_handler_id: None,
                    },
                )
            });
        let _ = std::fs::remove_dir_all(&root);

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("mismatched plugin manifest was resolved"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("example.reloaded")
                && error.to_string().contains("example.requested"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    fn provider_fixture(
        root: &Path,
        id: &str,
        enabled: bool,
        protocol: u32,
    ) -> crate::api::schema::InstalledPluginInfo {
        let plugin_root = root.join(id);
        std::fs::create_dir_all(&plugin_root).expect("create provider fixture");
        let manifest_path = plugin_root.join("herdr-plugin.toml");
        std::fs::write(
            &manifest_path,
            format!(
                r#"
id = "{id}"
name = "Provider"
version = "0.1.0"
min_herdr_version = "{}"
platforms = ["linux", "macos", "windows"]

[[execution_providers]]
scheme = "runtime"
protocol = {protocol}
pty_command = ["bin/provider", "connect"]
process_command = ["bin/provider", "exec"]
"#,
                crate::build_info::BASE_VERSION
            ),
        )
        .expect("write provider fixture");
        crate::app::load_plugin_manifest(&manifest_path.display().to_string(), enabled)
            .expect("load provider fixture")
    }

    #[cfg(unix)]
    #[test]
    fn resolves_one_enabled_compatible_execution_provider() {
        let root = std::env::temp_dir().join(format!(
            "herdr-provider-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        ));
        let plugin = provider_fixture(&root, "example.runtime", true, 1);
        let registry_path = root.join("plugins.json");
        crate::persist::plugin_registry::save_to_path(&registry_path, &[plugin])
            .expect("save provider registry");

        let resolved =
            crate::persist::plugin_registry::with_test_registry_path(registry_path, || {
                resolve_installed_execution_provider("runtime", ExecutionProviderCommandKind::Pty)
            })
            .expect("resolve provider");

        assert_eq!(resolved.plugin_id, "example.runtime");
        assert_eq!(resolved.command, ["bin/provider", "connect"]);
        assert_eq!(
            resolved.cwd,
            root.join("example.runtime").canonicalize().unwrap()
        );
        assert!(resolved
            .env
            .iter()
            .any(|(key, value)| key == "HERDR_PLUGIN_ID" && value == "example.runtime"));
        for key in [
            crate::integration::HERDR_WORKSPACE_ID_ENV_VAR,
            crate::integration::HERDR_TAB_ID_ENV_VAR,
            crate::integration::HERDR_PANE_ID_ENV_VAR,
            "HERDR_VIEW_ID",
            "HERDR_BUILD_BIN_PATH",
        ] {
            assert!(resolved.remove_env.iter().any(|removed| removed == key));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_disabled_and_ambiguous_execution_providers() {
        let root = std::env::temp_dir().join(format!(
            "herdr-provider-errors-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after Unix epoch")
                .as_nanos()
        ));
        let disabled = provider_fixture(&root, "example.disabled", false, 1);
        let registry_path = root.join("plugins.json");
        crate::persist::plugin_registry::save_to_path(&registry_path, &[disabled])
            .expect("save disabled provider registry");
        let disabled_error =
            crate::persist::plugin_registry::with_test_registry_path(registry_path.clone(), || {
                resolve_installed_execution_provider("runtime", ExecutionProviderCommandKind::Pty)
            })
            .unwrap_err();
        assert_eq!(disabled_error.kind(), std::io::ErrorKind::PermissionDenied);

        let first = provider_fixture(&root, "example.first", true, 1);
        let second = provider_fixture(&root, "example.second", true, 1);
        crate::persist::plugin_registry::save_to_path(&registry_path, &[first, second])
            .expect("save ambiguous provider registry");
        let ambiguous_error =
            crate::persist::plugin_registry::with_test_registry_path(registry_path, || {
                resolve_installed_execution_provider("runtime", ExecutionProviderCommandKind::Pty)
            })
            .unwrap_err();
        assert_eq!(ambiguous_error.kind(), std::io::ErrorKind::InvalidData);
        assert!(ambiguous_error.to_string().contains("ambiguous"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_batch_command_captures_output() {
        let path = std::env::temp_dir().join(format!(
            "herdr-plugin-command-output-{}.cmd",
            std::process::id()
        ));
        std::fs::write(&path, "@echo off\r\necho plugin-%1\r\n").expect("write batch fixture");
        let cwd = path.parent().expect("batch fixture parent");

        let output =
            command_for_argv_in_dir(&path.display().to_string(), &["ready".to_string()], cwd)
                .output()
                .expect("run batch fixture");
        let _ = std::fs::remove_file(&path);

        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "plugin-ready"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_explicit_relative_executable_runs_from_working_directory() {
        let root = std::env::temp_dir().join(format!(
            "herdr-plugin-relative-command-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create relative command fixture");
        let source = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("where.exe");
        let executable = root.join("tool.exe");
        std::fs::copy(source, &executable).expect("copy relative command fixture");

        let output = command_for_argv_in_dir("./tool.exe", &["/?".to_string()], &root)
            .output()
            .expect("run relative executable");
        let _ = std::fs::remove_dir_all(&root);

        assert!(output.status.success(), "{output:?}");
    }
}
