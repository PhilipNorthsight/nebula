use super::*;
use nebula_core::{TerminalId, TerminalTab, Worktree, WorktreeId};
use ratatui::backend::TestBackend;

fn snapshot(name: &str, path: &str) -> ServerEvent {
    ServerEvent::Snapshot {
        workspaces: vec![],
        active_workspace: Default::default(),
        projects: vec![Project {
            id: ProjectId("same-project".into()),
            name: name.into(),
            workspace_id: Default::default(),
            repo_path: path.into(),
            sort_order: 0,
        }],
        worktrees: vec![Worktree {
            id: WorktreeId("same-worktree".into()),
            project_id: ProjectId("same-project".into()),
            path: path.into(),
            branch: "main".into(),
            is_main: true,
            sort_order: 0,
        }],
        terminals: vec![TerminalTab {
            id: TerminalId("same-terminal".into()),
            worktree_id: WorktreeId("same-worktree".into()),
            name: "shell".into(),
            sort_order: 0,
            alive: true,
            run_command: None,
        }],
        agents: vec![],
        links: vec![],
        pr_seen: vec![],
        ui_state: None,
    }
}
fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn event(view: &mut View, peer: usize, event: ServerEvent) {
    view.message(Message {
        peer,
        generation: view.peers[peer].generation,
        event: Ok(event),
    });
}
fn fixture() -> (
    View,
    mpsc::Receiver<ClientRequest>,
    mpsc::Receiver<ClientRequest>,
) {
    let mut view = View::new(vec![RemoteProject {
        host: "fixture-box".into(),
        path: "/remote-repo".into(),
    }]);
    let (local, local_rx) = Connection::fixture();
    let (remote, remote_rx) = Connection::fixture();
    view.peers[0].connection = Some(local);
    view.peers[1].connection = Some(remote);
    event(&mut view, 0, snapshot("local-repo", "/local-repo"));
    event(&mut view, 1, snapshot("remote-repo", "/remote-repo"));
    (view, local_rx, remote_rx)
}

#[tokio::test]
async fn same_ids_on_two_hosts_never_cross_input_output_resize_or_detach() {
    let (mut view, mut local, mut remote) = fixture();
    view.attach();
    assert!(matches!(local.try_recv(), Ok(ClientRequest::Attach { .. })));
    view.key(key('j')); // remote project, detach local and attach remote
    assert!(matches!(local.try_recv(), Ok(ClientRequest::Detach { .. })));
    assert!(matches!(
        remote.try_recv(),
        Ok(ClientRequest::Attach { .. })
    ));
    view.focus = Focus::Terminal;
    view.key(key('x'));
    assert!(matches!(remote.try_recv(), Ok(ClientRequest::Input {data, ..}) if data == b"x"));
    view.resize(91, 29);
    assert!(matches!(
        remote.try_recv(),
        Ok(ClientRequest::Resize {
            cols: 91,
            rows: 29,
            ..
        })
    ));
    assert!(local.try_recv().is_err());
    let session = SessionRef::Terminal(TerminalId("same-terminal".into()));
    event(
        &mut view,
        0,
        ServerEvent::Output {
            session: session.clone(),
            seq: 0,
            data: b"WRONG-HOST".to_vec(),
        },
    );
    assert!(!view
        .term
        .as_ref()
        .unwrap()
        .parser
        .screen()
        .contents()
        .contains("WRONG-HOST"));
    event(
        &mut view,
        1,
        ServerEvent::Output {
            session,
            seq: 0,
            data: b"REMOTE".to_vec(),
        },
    );
    assert!(view
        .term
        .as_ref()
        .unwrap()
        .parser
        .screen()
        .contents()
        .contains("REMOTE"));
    view.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    view.key(key('k'));
    assert!(matches!(
        remote.try_recv(),
        Ok(ClientRequest::Detach { .. })
    ));
    assert!(matches!(local.try_recv(), Ok(ClientRequest::Attach { .. })));
}

#[tokio::test]
async fn disconnect_is_host_local_and_old_generation_input_is_not_replayed() {
    let (mut view, mut local, mut remote) = fixture();
    view.key(key('j'));
    remote.try_recv().unwrap();
    view.focus = Focus::Terminal;
    view.message(Message {
        peer: 1,
        generation: 0,
        event: Err("fixture connection lost".into()),
    });
    assert!(view.peers[0].online);
    assert!(!view.peers[1].online);
    assert_eq!(
        view.focus,
        Focus::Terminal,
        "disconnect must not reinterpret queued typing as panel navigation"
    );
    for key in [
        key('k'),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        key('x'),
    ] {
        view.key(key);
    }
    assert!(remote.try_recv().is_err());
    assert!(
        local.try_recv().is_err(),
        "typing after loss must not navigate into the local terminal"
    );
    view.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    view.key(key('k'));
    assert!(matches!(local.try_recv(), Ok(ClientRequest::Attach { .. })));
    let (replacement, mut replacement_rx) = Connection::fixture();
    view.peers[1].generation += 1;
    view.peers[1].connection = Some(replacement);
    event(&mut view, 1, snapshot("remote-repo", "/remote-repo"));
    view.message(Message {
        peer: 1,
        generation: 0,
        event: Err("stale disconnect".into()),
    });
    assert!(view.peers[1].online);
    assert!(
        replacement_rx.try_recv().is_err(),
        "reconnect must not replay input or auto-attach"
    );
    view.key(key('j'));
    assert!(matches!(
        replacement_rx.try_recv(),
        Ok(ClientRequest::Attach { .. })
    ));
    assert!(replacement_rx.try_recv().is_err());
}

#[tokio::test]
async fn removing_the_selected_session_discards_its_attachment_and_blocks_typing() {
    let (mut view, mut local, mut remote) = fixture();
    view.attach();
    local.try_recv().unwrap();
    view.focus = Focus::Terminal;
    event(
        &mut view,
        0,
        ServerEvent::EntityRemoved {
            id: EntityId::Terminal(TerminalId("same-terminal".into())),
        },
    );
    assert!(matches!(local.try_recv(), Ok(ClientRequest::Detach { .. })));
    assert!(view.term.is_none());
    view.key(key('x'));
    assert!(local.try_recv().is_err());
    assert!(remote.try_recv().is_err());
    assert_eq!(view.focus, Focus::Terminal);
}

#[tokio::test]
async fn remote_paths_and_file_events_have_no_local_action_surface() {
    let tmp = tempfile::tempdir().unwrap();
    let sentinel = tmp.path().join("sentinel");
    std::fs::write(&sentinel, "must stay local").unwrap();
    let (mut view, mut local, mut remote) = fixture();
    view.key(key('j'));
    remote.try_recv().unwrap();
    event(
        &mut view,
        1,
        ServerEvent::FilesOpened {
            agent: "some-agent".to_owned().into(),
            root: tmp.path().into(),
            paths: vec![sentinel.clone()],
        },
    );
    for c in ['g', 'f', 'F', 'b', 'c', 'i', 'r', 'n', 'e', '/'] {
        view.key(key(c));
    }
    assert!(local.try_recv().is_err());
    assert!(remote.try_recv().is_err());
    assert_eq!(
        std::fs::read_to_string(sentinel).unwrap(),
        "must stay local"
    );
}

#[tokio::test]
async fn a_real_render_keeps_both_projects_visible_during_remote_attach_and_loss() {
    let (mut view, _local, mut remote) = fixture();
    view.key(key('j'));
    remote.try_recv().unwrap();
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    terminal.draw(|f| view.draw(f)).unwrap();
    let screen = format!("{:?}", terminal.backend().buffer());
    assert!(screen.contains("local-repo [local]"));
    assert!(screen.contains("remote-repo [SSH fixture-box]"));
    view.offline(1, "lost".into());
    terminal.draw(|f| view.draw(f)).unwrap();
    let screen = format!("{:?}", terminal.backend().buffer());
    assert!(screen.contains("local-repo [local]"));
    assert!(screen.contains("OFFLINE"));
}

fn terminal_row(id: &str) -> TerminalTab {
    TerminalTab {
        id: TerminalId(id.into()),
        worktree_id: WorktreeId("same-worktree".into()),
        name: id.into(),
        sort_order: 0,
        alive: true,
        run_command: None,
    }
}

#[tokio::test]
async fn selection_follows_identity_when_preceding_rows_are_removed_and_inserted() {
    for attached in [false, true] {
        let (mut view, mut local, mut remote) = fixture();
        let mut initial = snapshot("local-repo", "/local-repo");
        if let ServerEvent::Snapshot { terminals, .. } = &mut initial {
            *terminals = vec![terminal_row("a"), terminal_row("b"), terminal_row("c")];
        }
        event(&mut view, 0, initial.clone());
        view.session = 1;
        let selected = SessionRef::Terminal(TerminalId("b".into()));
        if attached {
            view.attach();
            assert!(
                matches!(local.try_recv(), Ok(ClientRequest::Attach { session, .. }) if session == selected)
            );
            view.focus = Focus::Terminal;
        }
        event(
            &mut view,
            0,
            ServerEvent::EntityRemoved {
                id: EntityId::Terminal(TerminalId("a".into())),
            },
        );
        assert_eq!(view.session, 0);
        assert_eq!(view.sessions()[view.session].0, selected);
        event(&mut view, 0, initial);
        assert_eq!(view.session, 1);
        assert_eq!(view.sessions()[view.session].0, selected);
        assert!(local.try_recv().is_err());
        if attached {
            assert_eq!(view.term.as_ref().unwrap().sref, selected);
            view.key(key('x'));
            assert!(
                matches!(local.try_recv(), Ok(ClientRequest::Input { session, data }) if session == selected && data == b"x")
            );
        }
        assert!(remote.try_recv().is_err());
    }
}

fn attach_error() -> ServerEvent {
    ServerEvent::Error {
        req_id: None,
        message: "attach: executable unavailable".into(),
    }
}

#[tokio::test]
async fn failed_attach_blocks_typing_and_allows_explicit_retry() {
    let (mut view, mut local, mut remote) = fixture();
    view.key(key('j'));
    let session = SessionRef::Terminal(TerminalId("same-terminal".into()));
    assert!(matches!(
        remote.try_recv(),
        Ok(ClientRequest::Attach { .. })
    ));
    view.focus = Focus::Terminal;
    event(&mut view, 1, attach_error());
    assert!(view.term.as_ref().unwrap().exited);
    assert_eq!(view.focus, Focus::Terminal);
    view.key(key('x'));
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(remote.try_recv().is_err());
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    terminal.draw(|f| view.draw(f)).unwrap();
    assert!(format!("{:?}", terminal.backend().buffer()).contains("input blocked"));
    while remote.try_recv().is_ok() {}
    view.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        remote.try_recv(),
        Ok(ClientRequest::Detach { .. })
    ));
    assert!(
        matches!(remote.try_recv(), Ok(ClientRequest::Attach { session: retry, .. }) if retry == session)
    );
    event(
        &mut view,
        1,
        ServerEvent::Scrollback {
            session: session.clone(),
            base_seq: 0,
            data: b"ready".to_vec(),
        },
    );
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    view.key(key('x'));
    assert!(
        matches!(remote.try_recv(), Ok(ClientRequest::Input { session: input, data }) if input == session && data == b"x")
    );
    assert!(!view.term.as_ref().unwrap().exited);
    assert!(view.peers[1].pending_attaches.is_empty());
    assert!(local.try_recv().is_err());
}

#[tokio::test]
async fn errors_preserve_active_attachment_ownership() {
    let (mut view, mut local, mut remote) = fixture();
    view.attach();
    local.try_recv().unwrap();
    view.key(key('j'));
    remote.try_recv().unwrap();
    event(&mut view, 0, attach_error());
    assert!(!view.term.as_ref().unwrap().exited);
    view.peers[1].generation += 1;
    view.message(Message {
        peer: 1,
        generation: 0,
        event: Ok(attach_error()),
    });
    assert!(!view.term.as_ref().unwrap().exited);
    event(
        &mut view,
        1,
        ServerEvent::Error {
            req_id: None,
            message: "background warning".into(),
        },
    );
    assert!(!view.term.as_ref().unwrap().exited);
    event(
        &mut view,
        1,
        ServerEvent::EntityUpserted {
            entity: Entity::Terminal(terminal_row("second")),
        },
    );
    view.focus = Focus::Sessions;
    view.key(key('j'));
    assert!(matches!(
        remote.try_recv(),
        Ok(ClientRequest::Detach { .. })
    ));
    assert!(matches!(
        remote.try_recv(),
        Ok(ClientRequest::Attach { .. })
    ));
    event(&mut view, 1, attach_error());
    assert!(!view.term.as_ref().unwrap().exited);
    event(&mut view, 1, attach_error());
    assert!(view.term.as_ref().unwrap().exited);
    assert_eq!(view.active_peer, Some(1));
}
