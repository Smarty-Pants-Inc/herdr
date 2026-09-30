use super::*;

/// Run the headless server. This is the entry point called from main.rs.
pub fn run_server() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let handoff_import = args.get(2).map(String::as_str) == Some("--handoff-import");
    let process_context = crate::platform::prepare_server_process(handoff_import);
    init_logging();
    match process_context {
        Ok(true) => info!("server using persistent user service context"),
        Ok(false) => {}
        Err(err) => {
            warn!(%err, "could not select persistent user service context; retaining inherited context")
        }
    }
    crate::platform::raise_server_nofile_limit();

    if handoff_import {
        let socket_path = args
            .get(3)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff socket"))?;
        let token = args
            .get(4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff token"))?;
        return run_handoff_import_server(&socket_path, token);
    }
    // `herdr [--session S] server --import-from-running --expect-source-pid P ...`
    if let Some(at) = args.iter().position(|arg| arg == "--import-from-running") {
        let Some(params) = crate::cli::parse_live_handoff_params(&args[at + 1..]) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: herdr server --import-from-running --expect-source-pid <pid> [--expect-socket-inode <inode>] [--expected-protocol <n>] [--expected-version <version>]",
            ));
        };
        return run_pull_import_server(params);
    }

    let loaded_config = config::Config::load();
    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();
    let should_quit = Arc::new(AtomicBool::new(false));

    // Start the JSON API socket server.
    let _api_server = match api::start_server_with_stop_control(
        api_tx.clone(),
        event_hub.clone(),
        should_quit.clone(),
    ) {
        Ok(server) => server,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            eprintln!("error: herdr server is already running");
            eprintln!("api socket: {}", api::socket_path().display());
            std::process::exit(1);
        }
        Err(err) => return Err(err),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        // Create the App (with AppState, event channels, etc.).
        let mut app = app::App::new(
            &loaded_config.config,
            app::AppPolicy::PRODUCTION,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub,
        );
        seed_startup_workspace_if_empty(&mut app);

        // Create the headless server.
        let mut server = match HeadlessServer::new(
            app,
            &loaded_config.diagnostics,
            Some(api_tx.clone()),
            Some(_api_server),
            should_quit,
        ) {
            Ok(server) => server,
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                eprintln!("error: herdr server is already running");
                eprintln!("client socket: {}", client_socket_path().display());
                std::process::exit(1);
            }
            Err(err) => return Err(err),
        };

        info!(
            api_socket = %api::socket_path().display(),
            client_socket = %client_socket_path().display(),
            "herdr server started"
        );
        print_ready_message(&api::socket_path(), &client_socket_path());
        server.app.run_plugin_startup_hooks();

        server.run().await
    });

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

fn seed_startup_workspace_if_empty(app: &mut app::App) {
    let Some(cwd) = take_startup_cwd() else {
        return;
    };

    if !app.state.workspaces.is_empty() {
        info!(
            cwd = %cwd.display(),
            "restored session already has workspaces; ignoring startup cwd"
        );
        return;
    }

    match app.create_workspace_with_options(cwd.clone(), true) {
        Ok(_) => {
            info!(cwd = %cwd.display(), "created startup workspace");
        }
        Err(err) => {
            warn!(cwd = %cwd.display(), err = %err, "failed to create startup workspace");
            app.state.mode = app::Mode::Navigate;
        }
    }
}

fn take_startup_cwd() -> Option<PathBuf> {
    let cwd = std::env::var_os(crate::server::autodetect::STARTUP_CWD_ENV_VAR)?;
    std::env::remove_var(crate::server::autodetect::STARTUP_CWD_ENV_VAR);
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

#[cfg(unix)]
fn run_handoff_import_server(socket_path: &Path, token: &str) -> io::Result<()> {
    #[cfg(debug_assertions)]
    crate::server::handoff::start_test_owner_watchdog();
    let received = crate::server::handoff::receive(socket_path, token)?;
    serve_handoff_import(received)
}

/// Pulls the running server's panes into this process, which then serves them.
#[cfg(unix)]
fn run_pull_import_server(params: crate::api::schema::ServerLiveHandoffParams) -> io::Result<()> {
    #[cfg(debug_assertions)]
    crate::server::handoff::start_test_owner_watchdog();
    let received = crate::server::handoff::pull(params)?;
    serve_handoff_import(received)
}

#[cfg(unix)]
fn serve_handoff_import(mut received: crate::server::handoff::ReceivedHandoff) -> io::Result<()> {
    let loaded_config = config::Config::load();
    crate::server::handoff::log_import_result(received.manifest.panes.len());

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();
    let should_quit = Arc::new(AtomicBool::new(false));

    let mut imports = HashMap::new();
    for (pane, fd) in received.manifest.panes.into_iter().zip(received.fds) {
        let pane_id = pane.pane_id;
        imports.insert(
            pane_id,
            crate::handoff_runtime::ImportedHandoffRuntime {
                master_fd: fd,
                state: pane,
            },
        );
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let result = rt.block_on(async {
        let app = app::App::new_from_handoff(
            &loaded_config.config,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub.clone(),
            &received.manifest.snapshot,
            &mut imports,
        )?;
        crate::server::handoff::report_restored(&mut received.stream)?;
        if std::env::var("HERDR_TEST_HANDOFF_IMPORT_FAIL").as_deref() == Ok("after_restored") {
            return Err(io::Error::other(
                "test handoff import failure after restored",
            ));
        }
        wait_for_old_public_sockets_to_close(Duration::from_secs(5))?;
        if std::env::var("HERDR_TEST_HANDOFF_IMPORT_FAIL").as_deref() == Ok("hang_before_bind") {
            // A replacement that never binds the public paths or becomes ready.
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }

        let report_bound = received.report_bound_sockets;
        let api_server = api::start_server_with_stop_control(
            api_tx.clone(),
            event_hub.clone(),
            should_quit.clone(),
        )?;
        if report_bound {
            // Reported as soon as it is bound, so a source whose handoff fails
            // knows this socket file is ours to remove, and no other is.
            crate::server::handoff::report_bound(
                &mut received.stream,
                crate::server::handoff::BoundSocketKind::Api,
                api_server.identity(),
            )?;
        }
        let mut server = HeadlessServer::new(
            app,
            &loaded_config.diagnostics,
            Some(api_tx.clone()),
            Some(api_server),
            should_quit,
        )?;
        #[cfg(debug_assertions)]
        if report_bound
            && std::env::var("HERDR_TEST_HANDOFF_IMPORT_FAIL").as_deref()
                == Ok("split_client_report_before_ready")
        {
            // A replacement whose last report reaches the source in two
            // pieces, and which then never becomes ready.
            crate::server::handoff::report_bound_split_for_test(
                &mut received.stream,
                crate::server::handoff::BoundSocketKind::Client,
                &server.client_socket_identity,
            )?;
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        if report_bound {
            crate::server::handoff::report_bound(
                &mut received.stream,
                crate::server::handoff::BoundSocketKind::Client,
                &server.client_socket_identity,
            )?;
        }
        // Carried across before any client attaches, so the first title sent is
        // the override rather than the configured one it replaced.
        server.api_window_title = received.manifest.api_window_title.take();
        if std::env::var("HERDR_TEST_HANDOFF_IMPORT_FAIL").as_deref() == Ok("hang_before_ready") {
            // A replacement that holds the public sockets but never becomes ready.
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        crate::server::handoff::report_ready(&mut received.stream)?;
        crate::server::handoff::wait_committed(&mut received.stream)?;
        server.app.assume_handoff_ownership();
        server.app.unpause_handoff_readers();
        server.pending_handoff_repaint_nudge = true;
        if let Err(err) = crate::server::handoff::report_owned(&mut received.stream) {
            warn!(err = %err, "failed to report handoff ownership; continuing as owner");
        }
        info!("handoff import server started");
        print_ready_message(&api::socket_path(), &client_socket_path());
        server.app.run_plugin_startup_hooks();
        server.run().await
    });

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

#[cfg(not(unix))]
fn run_pull_import_server(_params: crate::api::schema::ServerLiveHandoffParams) -> io::Result<()> {
    Err(io::Error::other("live handoff is only supported on Unix"))
}

#[cfg(not(unix))]
fn run_handoff_import_server(_socket_path: &Path, _token: &str) -> io::Result<()> {
    Err(io::Error::other("live handoff is only supported on Unix"))
}

fn print_ready_message(api_socket: &Path, client_socket: &Path) {
    eprintln!("herdr server running; you can use any herdr CLI command in another terminal.");
    eprintln!("api socket: {}", api_socket.display());
    eprintln!("client socket: {}", client_socket.display());
    eprintln!(
        "logs: {}",
        crate::session::data_dir()
            .join("herdr-server.log")
            .display()
    );
    eprintln!("did you mean to open the Herdr TUI? run `herdr`; you do not need `herdr server`.");
}

/// Initialize logging for the server process.
fn init_logging() {
    crate::logging::init_file_logging("herdr-server.log");
}
