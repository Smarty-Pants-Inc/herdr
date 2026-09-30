//! Workspace default launch environment (`workspace.create --env`) reaches
//! every new pane and tab launched in that workspace through the common
//! launch path, which the TUI split/new-tab controls, the socket API and the
//! CLI all share. Each pane runs a real shell stand-in that records the value
//! it sees, keyed by its own `HERDR_PANE_ID`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::test_support::shutdown_test_runtimes;
use crate::api::schema::{
    Method, PaneSplitParams, PaneTarget, Request, ResponseResult, SplitDirection, SuccessResponse,
    TabCreateParams, WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceTarget,
};
use crate::app::App;
use crate::config::{Config, ShellModeConfig};

// A key the test host cannot already carry, so "unset" proves no inheritance.
const KEY: &str = "SMARTY_MEMBER_ENV_INHERIT_PROBE";
const UNSET: &str = "[<unset>]";

struct Harness {
    app: App,
    dir: PathBuf,
}

impl Harness {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "herdr-env-inherit-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let shell = dir.join("shell.sh");
        std::fs::write(
            &shell,
            format!(
                "#!/bin/sh\nf=\"{dir}/$(printf '%s' \"$HERDR_PANE_ID\" | tr ':' '_')\"\n\
                 printf '[%s]' \"${{{KEY}-<unset>}}\" > \"$f.tmp\" && mv \"$f.tmp\" \"$f\"\n\
                 exec sleep 60\n",
                dir = dir.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = shell.display().to_string();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        Self { app, dir }
    }

    fn call(&mut self, method: Method) -> ResponseResult {
        let response = self.app.handle_api_request(Request {
            id: "req".into(),
            method,
        });
        serde_json::from_str::<SuccessResponse>(&response)
            .unwrap_or_else(|_| panic!("expected success, got {response}"))
            .result
    }

    /// Returns (workspace_id, root_pane_id).
    fn create_workspace(&mut self, env: &[(&str, &str)]) -> (String, String) {
        match self.call(Method::WorkspaceCreate(WorkspaceCreateParams {
            source_workspace_id: None,
            cwd: Some(self.dir.display().to_string()),
            focus: true,
            label: None,
            env: env_map(env),
        })) {
            ResponseResult::WorkspaceCreated {
                workspace,
                root_pane,
                ..
            } => (workspace.workspace_id, root_pane.pane_id),
            other => panic!("unexpected workspace.create result: {other:?}"),
        }
    }

    fn split(
        &mut self,
        workspace_id: Option<&str>,
        target_pane_id: Option<&str>,
        env: &[(&str, &str)],
    ) -> String {
        match self.call(Method::PaneSplit(PaneSplitParams {
            workspace_id: workspace_id.map(str::to_string),
            target_pane_id: target_pane_id.map(str::to_string),
            direction: SplitDirection::Right,
            ratio: None,
            cwd: None,
            focus: false,
            right_click: Default::default(),
            env: env_map(env),
        })) {
            ResponseResult::PaneInfo { pane } => pane.pane_id,
            other => panic!("unexpected pane.split result: {other:?}"),
        }
    }

    fn tab(&mut self, workspace_id: Option<&str>, env: &[(&str, &str)]) -> String {
        match self.call(Method::TabCreate(TabCreateParams {
            workspace_id: workspace_id.map(str::to_string),
            cwd: None,
            focus: false,
            label: None,
            env: env_map(env),
        })) {
            ResponseResult::TabCreated { root_pane, .. } => root_pane.pane_id,
            other => panic!("unexpected tab.create result: {other:?}"),
        }
    }

    fn focus_pane(&mut self, pane_id: &str) {
        self.call(Method::PaneFocus(PaneTarget {
            pane_id: pane_id.into(),
        }));
    }

    fn focus_workspace(&mut self, workspace_id: &str) {
        self.call(Method::WorkspaceFocus(WorkspaceTarget {
            workspace_id: workspace_id.into(),
        }));
    }

    fn seen(&self, pane_id: &str) -> String {
        read_when_ready(&self.dir.join(pane_id.replace(':', "_")))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        shutdown_test_runtimes(&mut self.app);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn env_map(env: &[(&str, &str)]) -> HashMap<String, String> {
    env.iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn read_when_ready(path: &Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if !contents.is_empty() {
                return contents;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pane shell did not record its env at {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// ASK items 1 and 6: root, default split and default tab, implicit and explicit
// workspace selection.
#[tokio::test]
async fn workspace_env_is_default_for_new_splits_and_tabs() {
    let mut h = Harness::new("defaults");
    let (a, a_root) = h.create_workspace(&[(KEY, "probe-split")]);
    assert_eq!(h.seen(&a_root), "[probe-split]");

    let implicit_split = h.split(None, None, &[]);
    let implicit_tab = h.tab(None, &[]);
    let explicit_split = h.split(Some(&a), None, &[]);
    let target_split = h.split(None, Some(&a_root), &[]);
    let explicit_tab = h.tab(Some(&a), &[]);
    for pane in [
        &implicit_split,
        &implicit_tab,
        &explicit_split,
        &target_split,
        &explicit_tab,
    ] {
        assert!(pane.starts_with(&format!("{a}:")), "{pane} not in {a}");
        assert_eq!(h.seen(pane), "[probe-split]", "pane {pane}");
    }
}

// ASK item 3: explicit per-launch env wins (empty string included) and never
// relabels the workspace default, even with the overridden pane focused.
#[tokio::test]
async fn explicit_env_overrides_one_launch_only() {
    let mut h = Harness::new("override");
    let (_a, _a_root) = h.create_workspace(&[(KEY, "probe-split")]);

    let override_split = h.split(None, None, &[(KEY, "override")]);
    assert_eq!(h.seen(&override_split), "[override]");
    let override_tab = h.tab(None, &[(KEY, "override")]);
    assert_eq!(h.seen(&override_tab), "[override]");
    let empty_split = h.split(None, None, &[(KEY, "")]);
    assert_eq!(h.seen(&empty_split), "[]");
    let empty_tab = h.tab(None, &[(KEY, "")]);
    assert_eq!(h.seen(&empty_tab), "[]");

    h.focus_pane(&override_split);
    let default_split = h.split(None, None, &[]);
    let default_tab = h.tab(None, &[]);
    assert_eq!(h.seen(&default_split), "[probe-split]");
    assert_eq!(h.seen(&default_tab), "[probe-split]");
}

// ASK item 4: defaults belong to the workspace, not to its root pane.
#[tokio::test]
async fn defaults_survive_closing_the_root_pane() {
    let mut h = Harness::new("root-close");
    let (a, a_root) = h.create_workspace(&[(KEY, "probe-split")]);
    let survivor = h.split(None, None, &[]);
    h.call(Method::PaneClose(PaneTarget {
        pane_id: a_root.clone(),
    }));
    h.focus_pane(&survivor);

    let split = h.split(None, None, &[]);
    let tab = h.tab(Some(&a), &[]);
    assert_eq!(h.seen(&split), "[probe-split]");
    assert_eq!(h.seen(&tab), "[probe-split]");
}

// ASK items 5 and 6: a workspace without env stays unattributed; focus and
// target selection never mix workspaces; a later workspace that reuses the
// old public id does not pick up the closed workspace's defaults; snapshots
// never carry the env.
#[tokio::test]
async fn defaults_are_scoped_to_the_workspace_record() {
    let mut h = Harness::new("scope");
    let (a, a_root) = h.create_workspace(&[(KEY, "probe-split")]);
    let (b, b_root) = h.create_workspace(&[]);
    assert_eq!(h.seen(&b_root), UNSET);

    // B is focused: implicit launches land in B and stay unattributed.
    let b_split = h.split(None, None, &[]);
    let b_tab = h.tab(None, &[]);
    assert!(b_split.starts_with(&format!("{b}:")));
    assert_eq!(h.seen(&b_split), UNSET);
    assert_eq!(h.seen(&b_tab), UNSET);

    // B is still focused: explicit targeting of A resolves A's defaults.
    let a_split = h.split(Some(&a), None, &[]);
    let a_target_split = h.split(None, Some(&a_root), &[]);
    let a_tab = h.tab(Some(&a), &[]);
    assert_eq!(h.seen(&a_split), "[probe-split]");
    assert_eq!(h.seen(&a_target_split), "[probe-split]");
    assert_eq!(h.seen(&a_tab), "[probe-split]");

    // Focus A, then target B explicitly.
    h.focus_workspace(&a);
    let b_explicit = h.split(Some(&b), None, &[]);
    let b_explicit_tab = h.tab(Some(&b), &[]);
    let b_target = h.split(None, Some(&b_root), &[]);
    assert_eq!(h.seen(&b_explicit), UNSET);
    assert_eq!(h.seen(&b_explicit_tab), UNSET);
    assert_eq!(h.seen(&b_target), UNSET);
    let a_implicit = h.split(None, None, &[]);
    assert_eq!(h.seen(&a_implicit), "[probe-split]");

    let snapshot = crate::persist::capture(
        &h.app.state.workspaces,
        &h.app.state.terminals,
        &h.app.terminal_runtimes,
        h.app.state.active,
        h.app.state.selected,
    );
    let snapshot_json = serde_json::to_string(&snapshot).unwrap();
    assert!(!snapshot_json.contains("probe-split"));
    assert!(!snapshot_json.contains(KEY));

    // Close A, then a new workspace takes over A's public id (as a restart
    // that reuses ids would). It must not inherit A's defaults.
    h.call(Method::WorkspaceClose(WorkspaceCloseParams {
        workspace_id: a.clone(),
        close_group: false,
    }));
    let (c, _c_root) = h.create_workspace(&[]);
    let c_idx = h.app.parse_workspace_id(&c).unwrap();
    h.app.state.workspaces[c_idx].id = a.clone();
    // Drop A's records so reused pane ids cannot read stale captures.
    for entry in std::fs::read_dir(&h.dir).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{a}_"))
        {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    let reused_split = h.split(Some(&a), None, &[]);
    let reused_tab = h.tab(Some(&a), &[]);
    assert!(reused_split.starts_with(&format!("{a}:")));
    assert_eq!(h.seen(&reused_split), UNSET);
    assert_eq!(h.seen(&reused_tab), UNSET);
}
