use std::path::PathBuf;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;

use crate::api::schema::{Method, NotificationShowParams, NotificationShowSound, Request};
use crate::config::ToastHerdrPosition;
use crate::protocol::{
    self, ClientKeybindings, ClientLaunchMode, ClientMessage, NotificationActivation,
    RenderEncoding, ServerMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};
const NOTIFICATION_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn run_notification_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_notification_help();
        return Ok(2);
    };

    match subcommand {
        "show" => notification_show(&args[1..]),
        "activate" => notification_activate(&args[1..]),
        "help" | "--help" | "-h" => {
            print_notification_help();
            Ok(0)
        }
        _ => {
            print_notification_help();
            Ok(2)
        }
    }
}

fn notification_show(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_notification_show_args(args) {
        Ok(params) => params,
        Err(NotificationArgError::Usage) => {
            eprintln!(
                "usage: herdr notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]"
            );
            return Ok(2);
        }
        Err(NotificationArgError::Message(message)) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:notification:show".into(),
        method: Method::NotificationShow(params),
    })?)
}

fn notification_activate(args: &[String]) -> std::io::Result<i32> {
    let (socket_path, activation) = match parse_notification_activate_args(args) {
        Ok(parsed) => parsed,
        Err(NotificationArgError::Usage) => return Ok(2),
        Err(NotificationArgError::Message(message)) => {
            eprintln!("herdr notification activate: {message}");
            return Ok(2);
        }
    };

    activate_notification_at_with_timeout(&socket_path, activation, NOTIFICATION_ACTIVATION_TIMEOUT)
        .map(|activated| if activated { 0 } else { 1 })
}

fn activate_notification_at_with_timeout(
    socket_path: &std::path::Path,
    activation: NotificationActivation,
    read_timeout: Duration,
) -> std::io::Result<bool> {
    let mut stream = crate::ipc::connect_local_stream(socket_path)?;
    stream.set_nonblocking(false)?;
    match stream.set_recv_timeout(Some(read_timeout)) {
        Ok(()) => {}
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {}
        Err(error) => return Err(error),
    }

    protocol::write_message(
        &mut stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            cols: 0,
            rows: 0,
            cell_width_px: 0,
            cell_height_px: 0,
            requested_encoding: RenderEncoding::SemanticFrame,
            keybindings: ClientKeybindings::Server,
            launch_mode: ClientLaunchMode::NotificationActivator,
        },
    )
    .map_err(notification_activation_io_error)?;

    match protocol::read_message::<_, ServerMessage>(&mut stream, MAX_FRAME_SIZE)
        .map_err(notification_activation_io_error)?
    {
        ServerMessage::Welcome { version, .. } if version != PROTOCOL_VERSION => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("server protocol version {version} does not match {PROTOCOL_VERSION}"),
            ));
        }
        ServerMessage::Welcome {
            error: Some(error), ..
        } => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                error,
            ));
        }
        ServerMessage::Welcome { .. } => {}
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected Welcome message",
            ));
        }
    }

    protocol::write_message(
        &mut stream,
        &ClientMessage::ActivateNotification { activation },
    )
    .map_err(notification_activation_io_error)?;
    match protocol::read_message::<_, ServerMessage>(&mut stream, MAX_FRAME_SIZE)
        .map_err(notification_activation_io_error)?
    {
        ServerMessage::NotificationActivationProcessed { activated } => Ok(activated),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected notification activation result",
        )),
    }
}

fn notification_activation_io_error(error: protocol::FramingError) -> std::io::Error {
    match error {
        protocol::FramingError::Io(error) => error,
        protocol::FramingError::UnexpectedEof => std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "server closed connection",
        ),
        protocol::FramingError::Oversized { claimed, max } => std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("server message size {claimed} exceeds maximum {max}"),
        ),
        protocol::FramingError::Bincode(error) => {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NotificationArgError {
    Usage,
    Message(String),
}

fn parse_notification_activate_args(
    args: &[String],
) -> Result<(PathBuf, NotificationActivation), NotificationArgError> {
    let [socket_path, recipient_client_id, workspace_id, pane_id] = args else {
        return Err(NotificationArgError::Usage);
    };
    let recipient_client_id = recipient_client_id.parse().map_err(|_| {
        NotificationArgError::Message("recipient client ID must be an unsigned integer".into())
    })?;
    let pane_id = pane_id
        .parse()
        .map_err(|_| NotificationArgError::Message("pane ID must be an unsigned integer".into()))?;
    Ok((
        PathBuf::from(socket_path),
        NotificationActivation {
            recipient_client_id,
            workspace_id: workspace_id.clone(),
            pane_id,
        },
    ))
}

fn parse_notification_show_args(
    args: &[String],
) -> Result<NotificationShowParams, NotificationArgError> {
    let Some(title) = args.first().cloned() else {
        return Err(NotificationArgError::Usage);
    };
    if matches!(title.as_str(), "help" | "--help" | "-h") {
        return Err(NotificationArgError::Usage);
    }

    let mut body = None;
    let mut position = None;
    let mut sound = NotificationShowSound::None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--body" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationArgError::Message(
                        "missing value for --body".into(),
                    ));
                };
                body = Some(value.clone());
                index += 2;
            }
            "--position" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationArgError::Message(
                        "missing value for --position".into(),
                    ));
                };
                position = Some(parse_toast_position(value)?);
                index += 2;
            }
            "--sound" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationArgError::Message(
                        "missing value for --sound".into(),
                    ));
                };
                sound = parse_notification_sound(value)?;
                index += 2;
            }
            other => {
                return Err(NotificationArgError::Message(format!(
                    "unknown option: {other}"
                )));
            }
        }
    }

    Ok(NotificationShowParams {
        title,
        body,
        position,
        sound,
    })
}

fn parse_toast_position(value: &str) -> Result<ToastHerdrPosition, NotificationArgError> {
    match value {
        "top-left" => Ok(ToastHerdrPosition::TopLeft),
        "top-right" => Ok(ToastHerdrPosition::TopRight),
        "bottom-left" => Ok(ToastHerdrPosition::BottomLeft),
        "bottom-right" => Ok(ToastHerdrPosition::BottomRight),
        _ => Err(NotificationArgError::Message(format!(
            "invalid position: {value} (expected top-left, top-right, bottom-left, or bottom-right)"
        ))),
    }
}

fn parse_notification_sound(value: &str) -> Result<NotificationShowSound, NotificationArgError> {
    match value {
        "none" => Ok(NotificationShowSound::None),
        "done" => Ok(NotificationShowSound::Done),
        "request" => Ok(NotificationShowSound::Request),
        _ => Err(NotificationArgError::Message(format!(
            "invalid sound: {value} (expected none, done, or request)"
        ))),
    }
}

fn print_notification_help() {
    eprintln!("herdr notification commands:");
    eprintln!(
        "  herdr notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }
    struct TestSocketPath(PathBuf);

    impl Drop for TestSocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn activation_test_listener(name: &str) -> (crate::ipc::LocalListener, TestSocketPath) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        #[cfg(unix)]
        let path = {
            let _ = name;
            PathBuf::from(format!("/tmp/hna-{}-{nanos}.sock", std::process::id()))
        };
        #[cfg(windows)]
        let path = std::env::temp_dir().join(format!(
            "herdr-{name}-hna-{}-{nanos}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        (listener, TestSocketPath(path))
    }

    fn test_activation() -> NotificationActivation {
        NotificationActivation {
            recipient_client_id: 7,
            workspace_id: "workspace".to_owned(),
            pane_id: 42,
        }
    }

    fn receive_activation(
        stream: &mut crate::ipc::LocalStream,
        result: Option<bool>,
    ) -> NotificationActivation {
        assert!(matches!(
            protocol::read_message::<_, ClientMessage>(stream, MAX_FRAME_SIZE).unwrap(),
            ClientMessage::Hello {
                launch_mode: ClientLaunchMode::NotificationActivator,
                ..
            }
        ));
        protocol::write_message(
            stream,
            &ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                encoding: RenderEncoding::SemanticFrame,
                error: None,
            },
        )
        .unwrap();
        let activation =
            match protocol::read_message::<_, ClientMessage>(stream, MAX_FRAME_SIZE).unwrap() {
                ClientMessage::ActivateNotification { activation } => activation,
                other => panic!("expected activation, got {other:?}"),
            };
        if let Some(activated) = result {
            protocol::write_message(
                stream,
                &ServerMessage::NotificationActivationProcessed { activated },
            )
            .unwrap();
        }
        activation
    }

    #[test]
    fn notification_activate_returns_zero_for_processed_success() {
        use interprocess::local_socket::traits::Listener as _;

        let (listener, path) = activation_test_listener("accepted");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            receive_activation(&mut stream, Some(true))
        });
        let activation = test_activation();
        let command_args = vec![
            path.0.to_string_lossy().into_owned(),
            activation.recipient_client_id.to_string(),
            activation.workspace_id.clone(),
            activation.pane_id.to_string(),
        ];

        assert_eq!(notification_activate(&command_args).unwrap(), 0);
        assert_eq!(server.join().unwrap(), activation);
    }

    #[test]
    fn notification_activate_returns_nonzero_for_processed_rejection() {
        use interprocess::local_socket::traits::Listener as _;

        let (listener, path) = activation_test_listener("rejected");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            receive_activation(&mut stream, Some(false))
        });
        let activation = test_activation();
        let command_args = vec![
            path.0.to_string_lossy().into_owned(),
            activation.recipient_client_id.to_string(),
            activation.workspace_id.clone(),
            activation.pane_id.to_string(),
        ];

        assert_eq!(notification_activate(&command_args).unwrap(), 1);
        assert_eq!(server.join().unwrap(), activation);
    }

    #[cfg(unix)]
    #[test]
    fn unacknowledged_notification_activation_is_not_replayed() {
        use interprocess::local_socket::traits::Listener as _;
        use interprocess::local_socket::ListenerNonblockingMode;

        let (listener, path) = activation_test_listener("no-replay");
        let server = std::thread::spawn(move || {
            let first = {
                let mut stream = listener.accept().unwrap();
                receive_activation(&mut stream, None)
            };
            listener
                .set_nonblocking(ListenerNonblockingMode::Accept)
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_millis(100);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok(mut stream) => return (first, Some(receive_activation(&mut stream, None))),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("second accept failed: {error}"),
                }
            }
            (first, None)
        });
        let activation = test_activation();

        assert!(activate_notification_at_with_timeout(
            &path.0,
            activation.clone(),
            Duration::from_secs(1),
        )
        .is_err());
        assert_eq!(server.join().unwrap(), (activation, None));
    }

    #[cfg(unix)]
    #[test]
    fn unacknowledged_notification_activation_has_bounded_wait() {
        use interprocess::local_socket::traits::Listener as _;

        let (listener, path) = activation_test_listener("bounded");
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let activation = receive_activation(&mut stream, None);
            wait.recv().unwrap();
            activation
        });
        let activation = test_activation();

        assert!(activate_notification_at_with_timeout(
            &path.0,
            activation.clone(),
            Duration::from_millis(50),
        )
        .is_err());
        release.send(()).unwrap();
        assert_eq!(server.join().unwrap(), activation);
    }

    #[test]
    fn notification_activate_args_parse_explicit_socket_and_target() {
        assert_eq!(
            parse_notification_activate_args(&args(&[
                "/tmp/client socket;$(bad)",
                "42",
                "work space;$(bad)",
                "7",
            ])),
            Ok((
                PathBuf::from("/tmp/client socket;$(bad)"),
                NotificationActivation {
                    recipient_client_id: 42,
                    workspace_id: "work space;$(bad)".into(),
                    pane_id: 7,
                },
            ))
        );
    }

    #[test]
    fn notification_activate_args_reject_invalid_pane_id() {
        assert_eq!(
            parse_notification_activate_args(&args(&["/tmp/client.sock", "42", "work", "bad"])),
            Err(NotificationArgError::Message(
                "pane ID must be an unsigned integer".into(),
            ))
        );
    }

    #[test]
    fn notification_show_args_parse_title_body_and_position() {
        let params = parse_notification_show_args(&args(&[
            "build failed",
            "--body",
            "api workspace",
            "--position",
            "top-right",
            "--sound",
            "request",
        ]))
        .unwrap();

        assert_eq!(
            params,
            NotificationShowParams {
                title: "build failed".into(),
                body: Some("api workspace".into()),
                position: Some(ToastHerdrPosition::TopRight),
                sound: NotificationShowSound::Request,
            }
        );
    }

    #[test]
    fn notification_show_args_reject_invalid_position() {
        let error =
            parse_notification_show_args(&args(&["build failed", "--position", "top-center"]))
                .unwrap_err();

        assert_eq!(
            error,
            NotificationArgError::Message(
                "invalid position: top-center (expected top-left, top-right, bottom-left, or bottom-right)"
                    .into()
            )
        );
    }

    #[test]
    fn notification_show_args_default_sound_is_none() {
        let params = parse_notification_show_args(&args(&["build failed"])).unwrap();

        assert_eq!(params.sound, NotificationShowSound::None);
    }

    #[test]
    fn notification_show_args_reject_invalid_sound() {
        let error =
            parse_notification_show_args(&args(&["build failed", "--sound", "loud"])).unwrap_err();

        assert_eq!(
            error,
            NotificationArgError::Message(
                "invalid sound: loud (expected none, done, or request)".into()
            )
        );
    }
}
