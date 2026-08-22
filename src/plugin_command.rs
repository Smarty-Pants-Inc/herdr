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
