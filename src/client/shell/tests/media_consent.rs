use super::*;
use crate::input::TerminalKey;
use crossterm::event::{MouseButton, MouseEventKind};

fn consent_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_media_consent("m1".into(), "pane_1");
    state.compose(106, 24).unwrap();
    state
}

fn press(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(TerminalKey::new(
        code,
        KeyModifiers::NONE,
    ))])
}

fn click(state: &mut ClientShellState, column: u16, row: u16) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })])
}

#[test]
fn media_consent_answers_with_keys_and_keeps_keys_from_the_pane() {
    let mut state = consent_state();
    let text = frame_rows(&state.compose(106, 24).unwrap()).join("\n");
    assert!(text.contains("Allow microphone?"));
    assert!(text.contains("pane_1 wants to start a voice call"));

    let typed = press(&mut state, KeyCode::Char('x'));
    assert!(typed.requests.is_empty());
    assert!(typed.media_consent.is_empty());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::MediaConsent(_))
    ));

    let allowed = press(&mut state, KeyCode::Enter);
    assert_eq!(allowed.media_consent, vec![("m1".to_owned(), true)]);
    assert!(allowed.requests.is_empty());
    assert!(state.overlay.is_none());

    let mut state = consent_state();
    let denied = press(&mut state, KeyCode::Esc);
    assert_eq!(denied.media_consent, vec![("m1".to_owned(), false)]);
    assert!(state.overlay.is_none());
}

#[test]
fn media_consent_answers_with_the_primary_button_or_a_click_outside() {
    let mut state = consent_state();
    let primary = state.hits.overlay_primary;
    let allowed = click(&mut state, primary.x, primary.y);
    assert_eq!(allowed.media_consent, vec![("m1".to_owned(), true)]);

    let mut state = consent_state();
    let denied = click(&mut state, 0, 0);
    assert_eq!(denied.media_consent, vec![("m1".to_owned(), false)]);
    assert!(state.overlay.is_none());
}

#[test]
fn media_consent_closes_only_for_its_session() {
    let mut state = consent_state();
    assert!(!state.close_media_consent("other"));
    assert!(state.close_media_consent("m1"));
    assert!(state.overlay.is_none());
}

#[test]
fn media_pane_label_follows_the_focused_tab_and_zoom() {
    let mut projected = snapshot();
    let mut labelled = projected.panes[0].clone();
    labelled.pane_id = "pane_2".into();
    labelled.label = Some("voice agent".into());
    labelled.focused = false;
    let mut hidden = labelled.clone();
    hidden.pane_id = "pane_3".into();
    hidden.tab_id = "tab_2".into();
    let mut other_tab = projected.tabs[0].clone();
    other_tab.tab_id = "tab_2".into();
    other_tab.number = 2;
    other_tab.focused = false;
    projected.panes.extend([labelled, hidden]);
    projected.tabs.push(other_tab);

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected.clone()));
    assert_eq!(state.media_pane_label("pane_1").as_deref(), Some("pane_1"));
    assert_eq!(
        state.media_pane_label("pane_2").as_deref(),
        Some("voice agent")
    );
    assert_eq!(state.media_pane_label("pane_3"), None);

    projected.tabs[0].zoomed = true;
    projected.revision += 1;
    state.set_snapshot(Box::new(projected));
    assert_eq!(state.media_pane_label("pane_1").as_deref(), Some("pane_1"));
    assert_eq!(state.media_pane_label("pane_2"), None);
}
