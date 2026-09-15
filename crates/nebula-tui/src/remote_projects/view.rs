use super::{
    transport::{self, Connection, Message},
    RemoteProject,
};
use crate::app::{AttachedTerm, Tree};
use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use nebula_core::{ClientRequest, Entity, EntityId, Project, ProjectId, ServerEvent, SessionRef};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use tokio::sync::mpsc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Projects,
    Sessions,
    Terminal,
}
type ProjectKey = (usize, Option<ProjectId>);
struct Row {
    key: ProjectKey,
    label: String,
}
struct Peer {
    remote: Option<RemoteProject>,
    tree: Tree,
    generation: u64,
    online: bool,
    note: String,
    connection: Option<Connection>,
}

struct View {
    peers: Vec<Peer>,
    selected: Option<ProjectKey>,
    session: usize,
    focus: Focus,
    active_peer: Option<usize>,
    term: Option<AttachedTerm>,
    size: (u16, u16),
    flash: String,
}

impl View {
    fn new(remotes: Vec<RemoteProject>) -> Self {
        let peers = std::iter::once(None)
            .chain(remotes.into_iter().map(Some))
            .map(|remote| Peer {
                remote,
                tree: Tree::default(),
                generation: 0,
                online: false,
                note: "connecting".into(),
                connection: None,
            })
            .collect();
        Self {
            peers,
            selected: None,
            session: 0,
            focus: Focus::Projects,
            active_peer: None,
            term: None,
            size: (80, 24),
            flash: String::new(),
        }
    }

    fn connect(&mut self, peer: usize, events: mpsc::Sender<Message>) {
        if self.active_peer == Some(peer) {
            self.active_peer = None;
            self.term = None;
            self.focus = Focus::Projects;
        }
        let p = &mut self.peers[peer];
        p.connection = None; // Cancels the old socket, including its unsent input queue.
        p.generation += 1;
        p.online = false;
        p.note = "connecting".into();
        p.connection = Some(transport::connect(
            peer,
            p.generation,
            p.remote.as_ref().map(|r| r.host.clone()),
            nebula_core::paths::socket_path(),
            events,
        ));
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (peer, p) in self.peers.iter().enumerate() {
            let suffix = if p.online { "" } else { " · offline" };
            if let Some(remote) = &p.remote {
                let name = std::path::Path::new(&remote.path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                rows.push(Row {
                    key: (peer, None),
                    label: format!("{name} [SSH {}]{suffix}", remote.host),
                });
            } else {
                for project in &p.tree.projects {
                    rows.push(Row {
                        key: (peer, Some(project.id.clone())),
                        label: format!("{} [local]{suffix}", project.name),
                    });
                }
                if p.tree.projects.is_empty() {
                    rows.push(Row {
                        key: (peer, None),
                        label: format!("Local · no projects{suffix}"),
                    });
                }
            }
        }
        rows
    }

    fn project(&self) -> Option<(usize, &Project)> {
        let (peer, id) = self.selected.as_ref()?;
        let p = &self.peers[*peer];
        if let Some(remote) = &p.remote {
            let mut matches = p
                .tree
                .projects
                .iter()
                .filter(|p| p.repo_path == std::path::Path::new(&remote.path));
            let project = matches.next()?;
            // Same checkout in multiple workspaces must not silently choose one.
            if matches.next().is_some() {
                return None;
            }
            Some((*peer, project))
        } else {
            Some((
                *peer,
                p.tree
                    .projects
                    .iter()
                    .find(|p| Some(&p.id) == id.as_ref())?,
            ))
        }
    }

    fn sessions(&self) -> Vec<(SessionRef, String)> {
        let Some((peer, project)) = self.project() else {
            return Vec::new();
        };
        let tree = &self.peers[peer].tree;
        let mut rows = Vec::new();
        for wt in tree.worktrees.iter().filter(|w| w.project_id == project.id) {
            for agent in tree
                .agents
                .iter()
                .filter(|a| a.worktree_id == wt.id && !a.archived && a.cloud_session_id.is_none())
            {
                rows.push((
                    SessionRef::Agent(agent.id.clone()),
                    format!("{} · {}", wt.branch, agent.name),
                ));
            }
            for terminal in tree.terminals.iter().filter(|t| t.worktree_id == wt.id) {
                rows.push((
                    SessionRef::Terminal(terminal.id.clone()),
                    format!("{} · {}", wt.branch, terminal.name),
                ));
            }
        }
        rows
    }

    fn offline(&mut self, peer: usize, note: String) {
        let p = &mut self.peers[peer];
        p.online = false;
        p.generation += 1;
        p.note = note;
        p.connection = None;
        // Keep typing locked to this (now offline) peer. Automatically moving
        // to panels could reinterpret in-flight keystrokes as navigation into
        // the local terminal. Only explicit Ctrl+q unlocks after a disconnect.
    }

    fn send(&mut self, peer: usize, req: ClientRequest) {
        // No CRUD, filesystem operations, prewarm or global daemon commands.
        if !matches!(
            req,
            ClientRequest::Attach { .. }
                | ClientRequest::Detach { .. }
                | ClientRequest::Input { .. }
                | ClientRequest::Resize { .. }
        ) {
            return;
        }
        let p = &self.peers[peer];
        if !p.online {
            return;
        }
        let sent = p
            .connection
            .as_ref()
            .is_some_and(|c| c.tx.try_send(req).is_ok());
        if !sent {
            self.offline(
                peer,
                "connection busy or closed; input discarded; R reconnects".into(),
            );
        }
    }

    fn detach(&mut self) {
        if let (Some(peer), Some(term)) = (self.active_peer.take(), self.term.take()) {
            self.send(peer, ClientRequest::Detach { session: term.sref });
        }
    }

    fn attach(&mut self) {
        let Some((peer, _)) = self.project() else {
            return;
        };
        if !self.peers[peer].online {
            return;
        }
        let Some((session, _)) = self.sessions().get(self.session).cloned() else {
            return;
        };
        if self.active_peer == Some(peer)
            && self
                .term
                .as_ref()
                .is_some_and(|t| t.sref == session && !t.exited)
        {
            return;
        }
        self.detach();
        self.term = Some(AttachedTerm::new(session.clone(), self.size.0, self.size.1));
        self.active_peer = Some(peer);
        self.send(
            peer,
            ClientRequest::Attach {
                session,
                from_seq: None,
                cols: self.size.0,
                rows: self.size.1,
            },
        );
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let size = (cols.max(1), rows.max(1));
        if self.size == size {
            return;
        }
        self.size = size;
        if let (Some(peer), Some(term)) = (self.active_peer, &mut self.term) {
            term.resize(size.0, size.1);
            let session = term.sref.clone();
            self.send(
                peer,
                ClientRequest::Resize {
                    session,
                    cols: size.0,
                    rows: size.1,
                },
            );
        }
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.focus = Focus::Projects;
            return false;
        }
        if self.focus == Focus::Terminal {
            if let (Some(peer), Some(term)) = (self.active_peer, &self.term) {
                if !term.exited {
                    if let Some(data) = crate::keys::encode_key(&key, term.kitty_flags) {
                        let session = term.sref.clone();
                        self.send(peer, ClientRequest::Input { session, data });
                    }
                }
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left => {
                self.focus = if self.focus == Focus::Projects {
                    Focus::Sessions
                } else {
                    Focus::Projects
                };
            }
            KeyCode::Enter | KeyCode::Right => {
                self.attach();
                self.focus = if self.focus == Focus::Projects {
                    Focus::Sessions
                } else if self.term.is_some() {
                    Focus::Terminal
                } else {
                    Focus::Sessions
                };
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Up | KeyCode::Char('k') => {
                let down = matches!(key.code, KeyCode::Down | KeyCode::Char('j'));
                if self.focus == Focus::Projects {
                    let rows = self.rows();
                    let index = rows
                        .iter()
                        .position(|r| Some(&r.key) == self.selected.as_ref())
                        .unwrap_or(0);
                    let next = step(index, rows.len(), down);
                    self.detach();
                    self.selected = rows.get(next).map(|r| r.key.clone());
                    self.session = 0;
                    self.attach();
                } else {
                    self.session = step(self.session, self.sessions().len(), down);
                    self.attach();
                }
            }
            _ => {
                self.flash = "Prototype: session I/O only. File/git/forge tools stay in the owning terminal.".into();
            }
        }
        false
    }

    fn message(&mut self, message: Message) {
        if message.generation != self.peers[message.peer].generation {
            return;
        }
        let peer = message.peer;
        let event = match message.event {
            Ok(ev) => ev,
            Err(err) => {
                self.offline(peer, err);
                return;
            }
        };
        let p = &mut self.peers[peer];
        match event {
            ServerEvent::Snapshot {
                projects,
                worktrees,
                agents,
                terminals,
                ..
            } => {
                p.tree.projects = projects;
                p.tree.worktrees = worktrees;
                p.tree.agents = agents;
                p.tree.terminals = terminals;
                p.online = true;
                p.note.clear();
            }
            ServerEvent::EntityUpserted { entity } => match entity {
                Entity::Project(v) => upsert(&mut p.tree.projects, v, |v| &v.id),
                Entity::Worktree(v) => upsert(&mut p.tree.worktrees, v, |v| &v.id),
                Entity::Agent(v) => upsert(&mut p.tree.agents, v, |v| &v.id),
                Entity::Terminal(v) => upsert(&mut p.tree.terminals, v, |v| &v.id),
                _ => {}
            },
            ServerEvent::EntityRemoved { id } => match id {
                EntityId::Project(id) => p.tree.projects.retain(|p| p.id != id),
                EntityId::Worktree(id) => p.tree.worktrees.retain(|w| w.id != id),
                EntityId::Agent(id) => p.tree.agents.retain(|a| a.id != id),
                EntityId::Terminal(id) => p.tree.terminals.retain(|t| t.id != id),
                _ => {}
            },
            ServerEvent::Error { message, .. } => self.flash = message,
            // Crucially, FilesOpened is NOT handled: a remote path can never
            // fall through to the local editor, git, gh or a filesystem read.
            ServerEvent::FilesOpened { .. } => {
                self.flash =
                    "Open files inside the owning terminal (prototype has no file viewer).".into()
            }
            ev if self.active_peer == Some(peer) => {
                if let Some(term) = &mut self.term {
                    match ev {
                        ServerEvent::Scrollback {
                            session,
                            base_seq,
                            data,
                        } if session == term.sref => {
                            term.apply_scrollback(base_seq, &data);
                        }
                        ServerEvent::Output { session, seq, data } if session == term.sref => {
                            term.apply_output(seq, &data)
                        }
                        ServerEvent::KittyFlags { session, flags } if session == term.sref => {
                            term.kitty_flags = flags
                        }
                        ServerEvent::SessionExited { session, .. } if session == term.sref => {
                            term.exited = true
                        }
                        _ => {}
                    }
                    // No OSC clipboard forwarding from this intentionally limited view.
                    term.take_clipboard();
                }
            }
            _ => {}
        }
        let rows = self.rows();
        if !rows.iter().any(|r| Some(&r.key) == self.selected.as_ref()) {
            self.detach();
            self.selected = rows.first().map(|r| r.key.clone());
        }
        let sessions = self.sessions();
        if self
            .term
            .as_ref()
            .is_some_and(|term| !sessions.iter().any(|(id, _)| *id == term.sref))
        {
            self.detach();
        }
        self.session = self.session.min(sessions.len().saturating_sub(1));
    }

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .areas(f.area());
        f.render_widget(
            Paragraph::new(
                " NEBULA · mixed projects prototype · plain nebula keeps the full local TUI",
            ),
            header,
        );
        let [projects, sessions, pane] = Layout::horizontal([
            Constraint::Percentage(27),
            Constraint::Percentage(27),
            Constraint::Percentage(46),
        ])
        .areas(body);
        let rows = self.rows();
        let index = rows
            .iter()
            .position(|r| Some(&r.key) == self.selected.as_ref());
        render_list(
            f,
            projects,
            "PROJECTS",
            rows.iter().map(|r| r.label.clone()).collect(),
            index,
            self.focus == Focus::Projects,
        );
        let sessions_rows = self.sessions();
        render_list(
            f,
            sessions,
            "SESSIONS · worktree / name",
            sessions_rows.iter().map(|r| r.1.clone()).collect(),
            (!sessions_rows.is_empty()).then_some(self.session),
            self.focus == Focus::Sessions,
        );
        let peer = self.selected.as_ref().map(|k| k.0).unwrap_or(0);
        let status = if self.peers[peer].online {
            "connected"
        } else {
            "OFFLINE · R reconnect"
        };
        let title = if self.focus == Focus::Terminal
            && self.peers[peer].online
            && self.term.as_ref().is_some_and(|t| !t.exited)
        {
            format!("TERMINAL · typing · {status}")
        } else if self.focus == Focus::Terminal {
            format!("TERMINAL · input blocked · {status}")
        } else {
            format!("TERMINAL · {status}")
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(pane);
        f.render_widget(block, pane);
        self.resize(inner.width, inner.height);
        if let Some(term) = &self.term {
            f.render_widget(
                tui_term::widget::PseudoTerminal::new(term.parser.screen()),
                inner,
            );
        } else {
            let hint = if !self.peers[peer].online {
                self.peers[peer].note.as_str()
            } else if self.project().is_none() {
                "Checkout not registered (or ambiguous). Register its exact root in one remote workspace using nebula add on that host."
            } else {
                "Select an existing agent or terminal and Enter. Create sessions in normal nebula on the owning machine first."
            };
            f.render_widget(
                Paragraph::new(hint).wrap(ratatui::widgets::Wrap { trim: false }),
                inner,
            );
        }
        let note = if !self.peers[peer].online {
            self.peers[peer].note.as_str()
        } else {
            self.flash.as_str()
        };
        f.render_widget(Paragraph::new(format!("j/k or arrows select · Tab panels · Enter attach/type · Ctrl+q projects · q quit\nR reconnect selected host · no queued input replay · no file/git/forge actions\n{note}")), footer);
    }
}

fn step(index: usize, len: usize, down: bool) -> usize {
    if down {
        (index + 1).min(len.saturating_sub(1))
    } else {
        index.saturating_sub(1)
    }
}
fn upsert<T, K: PartialEq + ?Sized>(items: &mut Vec<T>, value: T, key: impl Fn(&T) -> &K) {
    if let Some(slot) = items.iter_mut().find(|v| key(v) == key(&value)) {
        *slot = value;
    } else {
        items.push(value);
    }
}
fn render_list(
    f: &mut Frame,
    area: Rect,
    title: &str,
    rows: Vec<String>,
    selected: Option<usize>,
    focused: bool,
) {
    let border = if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let list = List::new(rows.into_iter().map(ListItem::new).collect::<Vec<_>>())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(border)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");
    f.render_stateful_widget(
        list,
        area,
        &mut ListState::default().with_selected(selected),
    );
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub(super) async fn run(entries: Vec<RemoteProject>) -> Result<()> {
    let mut view = View::new(entries);
    let (tx, mut rx) = mpsc::channel(128);
    for peer in 0..view.peers.len() {
        view.connect(peer, tx.clone());
    }
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut input = EventStream::new();
    loop {
        terminal.draw(|f| view.draw(f))?;
        tokio::select! {
            Some(message) = rx.recv() => view.message(message),
            event = input.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    if view.focus != Focus::Terminal && key.code == KeyCode::Char('R') {
                        let peer = view.selected.as_ref().map(|k| k.0).unwrap_or(0);
                        view.connect(peer, tx.clone());
                    } else if view.key(key) { break; }
                }
                Some(Ok(Event::Resize(..))) => {},
                Some(Ok(_)) => {},
                Some(Err(err)) => return Err(err.into()),
                None => break,
            }
        }
        // Bound each paint batch so a chatty remote cannot starve local input.
        for _ in 0..64 {
            match rx.try_recv() {
                Ok(message) => view.message(message),
                Err(_) => break,
            }
        }
    }
    view.detach();
    Ok(())
}

#[cfg(test)]
mod tests;
