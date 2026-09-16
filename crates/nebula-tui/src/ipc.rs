//! Client-side IPC: connect to the daemon (auto-spawning it when absent) and
//! perform the version handshake.

use anyhow::{bail, Context, Result};
use nebula_core::codec::{read_frame, write_frame};
use nebula_core::{
    env, paths, AgentId, AgentKind, ClientRequest, EnterOutcome, ServerEvent, PROTOCOL_VERSION,
};
use std::time::Duration;
use tokio::net::UnixStream;

/// Request id for the one-shot CLIs: each opens a fresh connection, sends a
/// single request and waits for its reply, so there is never a second id.
const ONE_SHOT_REQ_ID: u64 = 1;
/// The daemon hung up mid-request — the message every one-shot client shows.
const CLOSED_BEFORE_REPLY: &str = "daemon closed the connection before replying";
/// How often the connect and shutdown waits re-check the daemon.
const POLL_STEP: Duration = Duration::from_millis(50);

pub struct Connection {
    pub stream: UnixStream,
    pub daemon_pid: u32,
}

/// Connect, auto-spawning `current_exe() daemon` when nothing is listening.
pub async fn connect_or_spawn() -> Result<Connection> {
    let sock = paths::socket_path();

    if let Ok(conn) = try_connect(&sock).await {
        return handshake(conn).await;
    }

    spawn_daemon()?;

    // Poll-connect while the daemon boots.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match try_connect(&sock).await {
            Ok(conn) => return handshake(conn).await,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(POLL_STEP).await;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "daemon did not come up on {} — check {}",
                        sock.display(),
                        paths::daemon_log_path().display()
                    )
                })
            }
        }
    }
}

async fn try_connect(sock: &std::path::Path) -> Result<UnixStream> {
    Ok(UnixStream::connect(sock).await?)
}

fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().context("resolve current_exe")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // New *session*, not just a new process group: besides outliving this
    // client and skipping its terminal signals (Ctrl+C etc.), the daemon must
    // hold no controlling terminal. It shells out to the user's interactive
    // shell (CLI probes, login-shell agent wrap), and an interactive zsh that
    // can reach a tty via /dev/tty grabs its foreground process group —
    // SIGTTIN-stopping the TUI running on this terminal mid-frame.
    unsafe {
        cmd.pre_exec(|| {
            if libc_setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("spawn nebula daemon")?;
    Ok(())
}

// Avoid a libc dependency for one call (same pattern as nebula-core's geteuid).
pub(crate) fn libc_setsid() -> i32 {
    extern "C" {
        fn setsid() -> i32;
    }
    unsafe { setsid() }
}

async fn handshake(mut stream: UnixStream) -> Result<Connection> {
    // Asked of the kernel before the daemon gets a word in: a daemon on
    // another protocol hangs up right after its answer.
    let listener_pid = peer_pid(&stream);
    let (mut reader, mut writer) = stream.split();
    let daemon_pid = hello(&mut reader, &mut writer, |version| {
        version_skew_message(version, listener_pid)
    })
    .await?;
    Ok(Connection { stream, daemon_pid })
}

/// The version handshake itself, over any pair of halves: the local socket,
/// or the stdio of an ssh whose far end is `nebula relay`. `skew` words the
/// refusal, since which fix applies depends on where the daemon is.
async fn hello<R, W>(
    reader: &mut R,
    writer: &mut W,
    skew: impl FnOnce(u32) -> String,
) -> Result<u32>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    write_frame(
        writer,
        &ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;
    match read_frame::<ServerEvent, _>(reader).await? {
        Some(ServerEvent::HelloOk { daemon_pid, .. }) => Ok(daemon_pid),
        Some(ServerEvent::Incompatible {
            daemon_protocol_version,
        }) => bail!(skew(daemon_protocol_version)),
        other => bail!("unexpected handshake reply: {other:?}"),
    }
}

/// The pid of the process listening on the far end of `stream`, from the
/// kernel's peer credentials (`SO_PEERCRED`, `LOCAL_PEEREPID`) — which need
/// neither a protocol the daemon speaks nor a pidfile that is still there.
fn peer_pid(stream: &UnixStream) -> Option<i32> {
    let pid = stream.peer_cred().ok()?.pid()?;
    (pid > 1 && pid.unsigned_abs() != std::process::id()).then_some(pid)
}

/// Explain a failed version handshake in terms of the fix.
///
/// Which side is stale decides the remedy, and getting it backwards costs a
/// debugging session: when the *daemon* is ahead, the `nebula` that just ran
/// is an older build than the one the daemon was launched from, and `nebula
/// kill` cannot help — the live instance immediately respawns its daemon
/// from its own binary (`spawn_daemon` above uses `current_exe`), so the
/// skew survives every restart. That is the common shape in a checkout,
/// where `make dev` runs `target/debug` while PATH still finds an older
/// `nebula` from the last `make install`.
///
/// `daemon_pid` comes off the socket, so the message can name the process
/// even when its pidfile is gone — and offer the plain SIGTERM that stops it
/// cleanly if `nebula kill` somehow can't, before anyone reaches for `-9`.
fn version_skew_message(daemon_protocol_version: u32, daemon_pid: Option<i32>) -> String {
    let client = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let daemon = match daemon_pid {
        Some(pid) => format!(
            "{} (pid {pid})",
            daemon_exe_path(pid).unwrap_or_else(|| "unknown".into())
        ),
        None => "unknown".into(),
    };
    let header = format!(
        "protocol mismatch: the daemon speaks v{daemon_protocol_version}, this client \
         v{PROTOCOL_VERSION}\n  this client: {client}\n  the daemon:  {daemon}\n"
    );
    if daemon_protocol_version > PROTOCOL_VERSION {
        format!(
            "{header}This client is the older build, so `nebula kill` will not fix it — the \
             running instance respawns its daemon from its own binary. Install the daemon's \
             build over this one instead (`make install` from that checkout)."
        )
    } else {
        let by_hand = daemon_pid
            .map(|pid| {
                format!(
                    " Should that fail, `kill {pid}` asks the daemon for the same clean \
                     shutdown — never `kill -9`, which skips flushing its database."
                )
            })
            .unwrap_or_default();
        format!(
            "{header}The daemon is the older build — run `nebula kill` and relaunch. That \
             stops every live session.{by_hand}"
        )
    }
}

/// Best-effort path of the binary the daemon at `pid` was launched from, so
/// the mismatch message can name it. Asked of the OS rather than the
/// handshake: `Incompatible` is what a *newer* daemon sends an older client,
/// so adding a field to it would only break decoding on the clients that
/// need this message most. The buildstamp beside the pidfile is no help
/// either — it is a content hash, not a path.
fn daemon_exe_path(pid: i32) -> Option<String> {
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        return Some(path.display().to_string());
    }
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// Channel-based IPC handle for the TUI event loop: outbound requests go
/// through `tx`; inbound events arrive on `rx`. Reader/writer tasks own the
/// socket halves.
pub struct IpcChannels {
    pub tx: tokio::sync::mpsc::Sender<ClientRequest>,
    pub rx: tokio::sync::mpsc::Receiver<ServerEvent>,
    /// The ssh carrying a REMOTE WORKSPACE's connection, kept so the far
    /// end lives exactly as long as these channels: dropping them (a switch
    /// back to a local workspace, or quitting) kills ssh, which ends the
    /// `nebula relay` on the remote — and nothing else there, the daemon
    /// least of all. None for the local socket.
    pub link: Option<tokio::process::Child>,
}

pub fn split_connection(conn: Connection) -> IpcChannels {
    let (read_half, write_half) = conn.stream.into_split();
    split_halves(read_half, write_half, None)
}

fn split_halves<R, W>(
    read_half: R,
    mut write_half: W,
    link: Option<tokio::process::Child>,
) -> IpcChannels
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<ServerEvent>(1024);
    let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<ClientRequest>(256);

    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(read_half);
        while let Ok(Some(ev)) = read_frame::<ServerEvent, _>(&mut reader).await {
            if event_tx.send(ev).await.is_err() {
                break;
            }
        }
        // Dropping event_tx closes the channel, signalling disconnect.
    });

    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            if write_frame(&mut write_half, &req).await.is_err() {
                break;
            }
        }
    });

    IpcChannels {
        tx: req_tx,
        rx: event_rx,
        link,
    }
}

/// Connect to the daemon on another machine, for a REMOTE WORKSPACE: one
/// `ssh host` whose remote command is the self-installing prelude every
/// `nebula ssh` uses, tailed with `nebula relay` — which connects to (or
/// spawns) the daemon there and pipes its socket over ssh's stdio. The
/// handshake then runs end to end, so what answers is that daemon itself,
/// and a protocol mismatch reads as one.
///
/// BatchMode: the TUI owns the terminal, so ssh must never prompt — a key
/// that needs a passphrase wants an agent, and an unknown host key wants a
/// `ssh host` by hand first. Its stderr is collected off the pipe, not the
/// screen, and the error names the last of it when the connection fails.
///
/// Settings deliberately do not ride along (as they do for `nebula ssh`):
/// nothing on the far end renders, so the remote's own settings are the
/// ones that matter there, and this side's keep applying to the screen.
pub async fn connect_remote(
    host: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<IpcChannels> {
    let cmd = relay_command(&nebula_core::remote_script::install_url(), env);
    let mut child = tokio::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-T", "--", host, &cmd])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("ssh not found on PATH — remote workspaces need the OpenSSH client")
            } else {
                anyhow::Error::from(e).context("failed to spawn ssh")
            }
        })?;
    let mut writer = child.stdin.take().context("ssh stdin")?;
    let mut reader = child.stdout.take().context("ssh stdout")?;
    let stderr = child.stderr.take().context("ssh stderr")?;
    // Whatever ssh and the remote script say — "installing...", a refused
    // key, "too old" — is the diagnosis when the handshake fails.
    let said = tokio::spawn(async move {
        let mut text = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(
            &mut tokio::io::BufReader::new(stderr),
            &mut text,
        )
        .await;
        text
    });
    let handshake = tokio::time::timeout(
        REMOTE_CONNECT_TIMEOUT,
        hello(&mut reader, &mut writer, |version| {
            remote_skew_message(host, version)
        }),
    );
    match handshake.await {
        Ok(Ok(_pid)) => {
            // Keep draining stderr so a chatty ssh can't block on the pipe.
            tokio::spawn(async move {
                let _ = said.await;
            });
            Ok(split_halves(reader, writer, Some(child)))
        }
        Ok(Err(err)) => {
            // Kill first, so the stderr collector sees EOF and returns.
            let _ = child.kill().await;
            let said = said.await.unwrap_or_default();
            let said = last_lines(&said, 3);
            if said.is_empty() {
                Err(err.context(format!("{host}: connection failed")))
            } else {
                Err(err.context(format!("{host}: {said}")))
            }
        }
        Err(_) => {
            let _ = child.kill().await;
            let said = last_lines(&said.await.unwrap_or_default(), 3);
            if said.is_empty() {
                bail!(
                    "{host}: no answer within {}s",
                    REMOTE_CONNECT_TIMEOUT.as_secs()
                )
            } else {
                bail!(
                    "{host}: no answer within {}s — {said}",
                    REMOTE_CONNECT_TIMEOUT.as_secs()
                )
            }
        }
    }
}

/// Long enough for a first connection to install nebula on a bare box.
const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// The tail of a stderr transcript, joined on one line for a flash.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" · ")
}

/// A remote on another protocol version gets a message about *that*
/// machine — the local skew text's `nebula kill` advice would stop the
/// wrong daemon.
fn remote_skew_message(host: &str, daemon_protocol_version: u32) -> String {
    let fix = if daemon_protocol_version > PROTOCOL_VERSION {
        "upgrade nebula here".to_string()
    } else {
        format!("open it with `nebula ssh {host}` and run `nebula upgrade` there")
    };
    format!(
        "{host} runs nebula protocol v{daemon_protocol_version}, this one v{PROTOCOL_VERSION} — {fix}"
    )
}

/// Runs under `sh -c` on the remote: $1 = install URL, $2… = `NAME=value`
/// pairs to export first (the binding's `env`), each one positional and
/// single-quoted like the URL, so the script itself never carries a
/// value. The `--help` probe is the version check, as in `nebula tunnel`:
/// the prelude only installs nebula when the remote has none, so a box
/// last touched a few releases ago has a nebula that predates `relay` and
/// would fail on the unknown command with a clap usage dump. Naming the
/// fix beats that.
const RELAY_SCRIPT: &str = concat!(
    nebula_core::install_prelude!(),
    "shift; for kv in \"$@\"; do export \"$kv\"; done; ",
    "nebula relay --help >/dev/null 2>&1 || { ",
    "echo \"nebula: the nebula on this host is too old for remote workspaces; ",
    "reach it with nebula ssh and run nebula upgrade there\" >&2; exit 1; }; ",
    "exec nebula relay"
);

fn relay_command(install_url: &str, env: &std::collections::BTreeMap<String, String>) -> String {
    use nebula_core::remote_script::shell_single_quote;
    let mut cmd = format!(
        "sh -c '{}' nebula-relay {}",
        RELAY_SCRIPT,
        shell_single_quote(install_url)
    );
    for (name, value) in env {
        cmd.push(' ');
        cmd.push_str(&shell_single_quote(&format!("{name}={value}")));
    }
    cmd
}

/// `nebula relay`, the far end of [`connect_remote`]: connect to this
/// machine's daemon — spawning it when nothing is listening, exactly as the
/// TUI would — and pipe the socket over stdin/stdout until either side
/// hangs up. No handshake of its own: the frames are the client's, and the
/// client is the one that needs the answer.
pub async fn relay_stdio() -> Result<()> {
    let sock = paths::socket_path();
    let stream = match try_connect(&sock).await {
        Ok(stream) => stream,
        Err(_) => {
            spawn_daemon()?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            loop {
                match try_connect(&sock).await {
                    Ok(stream) => break stream,
                    Err(_) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(POLL_STEP).await;
                    }
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!("daemon did not come up on {}", sock.display())
                        })
                    }
                }
            }
        }
    };
    let (mut sock_rd, mut sock_wr) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    // Either direction closing ends the relay: the client hung up (ssh
    // died, or the TUI switched away), or the daemon did.
    tokio::select! {
        r = tokio::io::copy(&mut stdin, &mut sock_wr) => { r.context("client → daemon")?; }
        r = tokio::io::copy(&mut sock_rd, &mut stdout) => { r.context("daemon → client")?; }
    }
    Ok(())
}

/// The agent id a one-shot CLI runs as, from the raw `NEBULA_AGENT_ID`
/// value. Unset and empty are the same miss, and the error names the
/// `nebula <verb>` that needs it. Pure so the message is testable.
fn agent_id_from(value: Option<String>, verb: &str) -> Result<String> {
    value.filter(|v| !v.is_empty()).with_context(|| {
        format!(
            "{} is not set — `nebula {verb}` only works from inside a \
             nebula agent session",
            env::AGENT_ID
        )
    })
}

/// [`agent_id_from`] over the live environment.
fn current_agent_id(verb: &str) -> Result<String> {
    agent_id_from(env::non_empty(env::AGENT_ID), verb)
}

/// What the daemon said back to a one-shot request.
enum Reply {
    Ack,
    /// The daemon's own message for a request it declined.
    Error(String),
}

/// Read events until the daemon answers `req_id` — an Ack or an Error —
/// skipping anything else it broadcasts meanwhile. Only the hang-up is an
/// `Err`; the daemon's refusal comes back as a [`Reply::Error`] so each
/// caller can decide whether that is a failure.
async fn await_reply(conn: &mut Connection, req_id: u64) -> Result<Reply> {
    loop {
        match read_frame::<ServerEvent, _>(&mut conn.stream).await? {
            Some(ServerEvent::Ack { req_id: r, .. }) if r == req_id => return Ok(Reply::Ack),
            Some(ServerEvent::Error {
                req_id: Some(r),
                message,
            }) if r == req_id => return Ok(Reply::Error(message)),
            Some(_) => continue,
            None => bail!("{CLOSED_BEFORE_REPLY}"),
        }
    }
}

/// [`await_reply`] for the callers where the daemon declining *is* the
/// failure: its message becomes the error.
async fn await_ack(conn: &mut Connection, req_id: u64) -> Result<()> {
    match await_reply(conn, req_id).await? {
        Reply::Ack => Ok(()),
        Reply::Error(message) => bail!("{message}"),
    }
}

/// How `nebula rename` treats a session that already carries a title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    /// Title only an untitled session; the daemon declines otherwise.
    Auto,
    /// Overwrite whatever title the session has (`--force`).
    Force,
}

/// One-shot client for `nebula rename`, run from inside an agent session's
/// CLI: resolve the agent from NEBULA_AGENT_ID and ask the daemon to title
/// it. Never spawns a daemon — no daemon means no session worth titling.
///
/// Daemon-reported outcomes (renamed, or "already titled" on the non-force
/// path) both print and exit 0: for the model running this, a declined
/// auto-title is a settled answer, not a failure to retry.
pub async fn rename_current_agent(title: &str, mode: RenameMode) -> Result<()> {
    let agent_id = current_agent_id("rename")?;
    let sock = paths::socket_path();
    let Ok(stream) = try_connect(&sock).await else {
        bail!("no nebula daemon is running — title unchanged");
    };
    let mut conn = handshake(stream).await?;
    let req_id = ONE_SHOT_REQ_ID;
    let id = AgentId(agent_id);
    let name = title.to_string();
    let request = match mode {
        RenameMode::Force => ClientRequest::RenameAgent { req_id, id, name },
        RenameMode::Auto => ClientRequest::AutoRenameAgent { req_id, id, name },
    };
    write_frame(&mut conn.stream, &request).await?;
    match await_reply(&mut conn, req_id).await? {
        Reply::Ack => println!("session renamed to \"{title}\""),
        Reply::Error(message) => println!("nebula: {message}"),
    }
    Ok(())
}

/// CLI: `nebula spawn "<task>" [--kind <claude|codex|cursor>]` from inside
/// an agent session — ask the daemon to start a new agent beside this one,
/// in the same worktree, opening on `task` as its first prompt. The caller
/// is untouched: no relocation, no turn-end wait. Never spawns a daemon: no
/// daemon means no session to sit beside.
///
/// What this prints is read by the model that ran it, so it says what
/// happened and that this session carries on. A daemon-side refusal (a
/// blank task, a missing CLI) is a nonzero exit the model reports.
pub async fn spawn_sibling_for_current_agent(task: &str, kind: Option<AgentKind>) -> Result<()> {
    let agent_id = current_agent_id("spawn")?;
    let task = task.trim();
    if task.is_empty() {
        bail!("the task is empty — `nebula spawn \"<task>\"` needs the work the new session starts on");
    }
    let sock = paths::socket_path();
    let Ok(stream) = try_connect(&sock).await else {
        bail!("no nebula daemon is running — no session started");
    };
    let mut conn = handshake(stream).await?;
    let req_id = ONE_SHOT_REQ_ID;
    write_frame(
        &mut conn.stream,
        &ClientRequest::SpawnSiblingAgent {
            req_id,
            id: AgentId(agent_id),
            kind,
            starting_prompt: task.to_string(),
        },
    )
    .await?;
    await_ack(&mut conn, req_id).await?;
    let harness = kind.map(|k| format!("{} ", k.as_str())).unwrap_or_default();
    println!(
        "started a new {harness}session in this worktree; it is working on that task now and \
         shows in the sessions list. This session is unaffected — carry on."
    );
    Ok(())
}

/// CLI: `nebula open <file>…` from inside an agent session — resolve the
/// paths here, where the cwd is the agent's, and hand them to the daemon,
/// which raises every attached TUI's FILE TABS on them. Never spawns a
/// daemon: no daemon means nobody is looking. What this prints is read by
/// the model that ran it.
pub async fn open_files_for_current_agent(files: &[String]) -> Result<()> {
    let agent_id = current_agent_id("open")?;
    if files.is_empty() {
        bail!("nothing to open — `nebula open <file>…` needs at least one file");
    }
    let mut resolved = Vec::with_capacity(files.len());
    for file in files {
        let path = std::fs::canonicalize(file)
            .with_context(|| format!("can't open {file}: no such file"))?;
        if !path.is_file() {
            bail!("can't open {file}: not a file");
        }
        // A terminal shows text: a tab of a PNG's bytes shows nobody
        // anything, so the file is refused here — the model names the
        // path in its reply instead — by git's NUL-in-the-head test.
        let text = crate::tree_browser::is_text_file(&path)
            .with_context(|| format!("can't open {file}: unreadable"))?;
        if !text {
            bail!(
                "can't open {file}: not a text file — nebula shows text only (no images, PDFs or \
                 other binaries); name the path in your reply instead"
            );
        }
        resolved.push(path);
    }
    let sock = paths::socket_path();
    let Ok(stream) = try_connect(&sock).await else {
        bail!("no nebula daemon is running — nothing opened");
    };
    let mut conn = handshake(stream).await?;
    let req_id = ONE_SHOT_REQ_ID;
    let count = resolved.len();
    write_frame(
        &mut conn.stream,
        &ClientRequest::OpenFiles {
            req_id,
            id: AgentId(agent_id),
            paths: resolved,
        },
    )
    .await?;
    await_ack(&mut conn, req_id).await?;
    let noun = if count == 1 { "file" } else { "files" };
    println!(
        "opened {count} {noun} in nebula's file tabs — the user is looking at them now, one tab \
         each, with a preview and an editor. Don't paste their contents into your reply; carry on."
    );
    Ok(())
}

/// CLI: `nebula worktree [name] [--base <ref>]` from inside an agent
/// session — take (or create) the named worktree of this session's project
/// and have the daemon move the session into it. A blank name is invented
/// the way the TUI's new-worktree prompt invents one, and spaces slugify to
/// hyphens the same way. Never spawns a daemon: no daemon means no session
/// to move.
///
/// What this prints is read by the model that ran it, so the relocating
/// case spells out what happens next: the daemon kills and resumes this
/// very process once the turn ends, and the answer has to be finished by
/// then. A daemon-side failure is a nonzero exit — the model reports it and
/// stays put.
pub async fn enter_worktree_for_current_agent(name: &str, base: Option<String>) -> Result<()> {
    let agent_id = current_agent_id("worktree")?;
    let branch = match crate::branch_name::slugify(name) {
        slug if slug.is_empty() => crate::branch_name::random_name(&[]),
        slug => slug,
    };
    let sock = paths::socket_path();
    let Ok(stream) = try_connect(&sock).await else {
        bail!("no nebula daemon is running — nothing to move");
    };
    let mut conn = handshake(stream).await?;
    let req_id = ONE_SHOT_REQ_ID;
    write_frame(
        &mut conn.stream,
        &ClientRequest::EnterWorktree {
            req_id,
            id: AgentId(agent_id),
            branch,
            base,
        },
    )
    .await?;
    loop {
        match read_frame::<ServerEvent, _>(&mut conn.stream).await? {
            Some(ServerEvent::WorktreeEntered {
                req_id: r,
                worktree,
                outcome,
            }) if r == req_id => {
                println!(
                    "worktree \"{}\" is ready at {}",
                    worktree.branch,
                    worktree.path.display()
                );
                match outcome {
                    EnterOutcome::AlreadyThere => {
                        println!("this session already runs inside it — nothing to move.");
                    }
                    EnterOutcome::Relocating => {
                        println!(
                            "this session is now associated with it; nebula will relocate the \
                             session into it the moment this turn ends."
                        );
                        println!(
                            "Finish now: tell the user in one line that the session is moving \
                             into the worktree, and make no further tool calls or edits — you \
                             will be resumed inside the worktree with a prompt to continue."
                        );
                    }
                    EnterOutcome::NextLaunch => {
                        println!(
                            "this session is now associated with it and runs there from its \
                             next launch."
                        );
                    }
                }
                return Ok(());
            }
            Some(ServerEvent::Error {
                req_id: Some(r),
                message,
            }) if r == req_id => bail!("{message}"),
            Some(_) => continue,
            None => bail!("{CLOSED_BEFORE_REPLY}"),
        }
    }
}

/// One-shot client for `nebula add <dir>` (and bare `nebula <dir>`): resolve
/// the path locally — the daemon's cwd is not ours, so relative paths must be
/// absolutized here — and ask the daemon to register it as a project. The
/// daemon owns the rest: normalizing to the repo toplevel, naming the project
/// after the directory, rejecting non-repos and duplicates. Spawns a daemon
/// when none is running, same as launching the TUI would.
pub async fn add_project(path: &str) -> Result<()> {
    let expanded = match (path.strip_prefix("~/"), env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => std::path::PathBuf::from(path),
    };
    let dir = std::fs::canonicalize(&expanded)
        .with_context(|| format!("{} does not exist", expanded.display()))?;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let mut conn = connect_or_spawn().await?;
    let req_id = ONE_SHOT_REQ_ID;
    write_frame(
        &mut conn.stream,
        &ClientRequest::AddProject {
            req_id,
            path: dir.clone(),
            name: None,
            create_missing: false,
        },
    )
    .await?;
    await_ack(&mut conn, req_id).await?;
    println!("added project {}", dir.display());
    Ok(())
}

/// One `nebula workspace <op>` invocation, resolved and executed against the
/// daemon (spawned when absent, same as `nebula add`).
#[derive(Debug, Clone)]
pub enum WorkspaceOp {
    Add {
        name: String,
    },
    Open {
        name: String,
    },
    List,
    Delete {
        name: String,
    },
    Rename {
        name: String,
        new_name: String,
    },
    /// Bind the workspace to an ssh destination (`None` unbinds) — see
    /// `workspace_hosts`. Resolves the name against the daemon like the
    /// others, then writes this machine's binding file. `env` is exported
    /// on the remote before its nebula runs; given with no host, the
    /// existing host is kept.
    Host {
        name: String,
        host: Option<String>,
        env: Vec<String>,
    },
}

/// One-shot client for `nebula workspace …`. Name→id resolution runs off a
/// snapshot (Subscribe's first reply), so the daemon's RPC surface stays
/// id-based for the TUI's picker.
pub async fn run_workspace_op(op: WorkspaceOp) -> Result<()> {
    use nebula_core::{Workspace, WorkspaceId};
    let mut conn = connect_or_spawn().await?;
    write_frame(&mut conn.stream, &ClientRequest::Subscribe).await?;
    let (workspaces, active, projects) = loop {
        match read_frame::<ServerEvent, _>(&mut conn.stream).await? {
            Some(ServerEvent::Snapshot {
                workspaces,
                active_workspace,
                projects,
                ..
            }) => break (workspaces, active_workspace, projects),
            Some(_) => continue,
            None => bail!("daemon closed the connection before sending a snapshot"),
        }
    };
    let resolve = |name: &str| -> Result<WorkspaceId> {
        workspaces
            .iter()
            .find(|w: &&Workspace| w.name == name)
            .map(|w| w.id.clone())
            .with_context(|| {
                let names: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
                format!(
                    "no workspace named '{name}' (available: {})",
                    names.join(", ")
                )
            })
    };
    let req_id = ONE_SHOT_REQ_ID;
    let (request, done): (ClientRequest, String) = match op {
        WorkspaceOp::List => {
            let bindings = crate::workspace_hosts::load();
            for w in &workspaces {
                let marker = if w.id == active { "*" } else { " " };
                let count = projects.iter().filter(|p| p.workspace_id == w.id).count();
                let host = bindings
                    .get(w.id.as_str())
                    .map(|b| format!("  @{}", b.host()))
                    .unwrap_or_default();
                println!(
                    "{marker} {}  ({count} project{}){host}",
                    w.name,
                    if count == 1 { "" } else { "s" }
                );
            }
            return Ok(());
        }
        WorkspaceOp::Host { name, host, env } => {
            use crate::workspace_hosts::{binding_for, parse_env, parse_host, Binding};
            let id = resolve(&name)?;
            let host = match host.as_deref() {
                Some(text) => parse_host(text).map_err(|e| anyhow::anyhow!(e))?,
                None => None,
            };
            let env = env
                .iter()
                .map(|kv| parse_env(kv).map_err(|e| anyhow::anyhow!(e)))
                .collect::<Result<_>>()?;
            // `--env` alone re-binds the current host with new variables;
            // `--clear` (no host, no env) unbinds.
            let binding = match (host, env) {
                (Some(host), env) => Some(Binding::new(host, env)),
                (None, env) if !env.is_empty() => {
                    let current = binding_for(&id).with_context(|| {
                        format!("workspace '{name}' is not bound — name the host as well")
                    })?;
                    Some(Binding::new(current.host().to_string(), env))
                }
                (None, _) => None,
            };
            crate::workspace_hosts::set(&id, binding.as_ref())
                .context("failed to write workspace_hosts.json")?;
            match binding {
                Some(b) if b.env().is_empty() => {
                    println!("workspace '{name}' now shows {}'s projects", b.host())
                }
                Some(b) => println!(
                    "workspace '{name}' now shows {}'s projects, with {} exported there first",
                    b.host(),
                    b.env().keys().cloned().collect::<Vec<_>>().join(", ")
                ),
                None => println!("workspace '{name}' is local again"),
            }
            return Ok(());
        }
        WorkspaceOp::Add { name } => (
            ClientRequest::AddWorkspace {
                req_id,
                name: name.clone(),
            },
            format!("workspace '{name}' added — open it with `nebula workspace open {name}`"),
        ),
        WorkspaceOp::Open { name } => (
            ClientRequest::OpenWorkspace {
                req_id,
                id: resolve(&name)?,
            },
            // Running instances keep the workspace their user put them on;
            // this sets where the next one starts. `nebula --workspace
            // <name>` is the way to aim one instance without moving this.
            format!("workspace '{name}' will open in new nebula instances"),
        ),
        WorkspaceOp::Delete { name } => (
            ClientRequest::RemoveWorkspace {
                req_id,
                id: resolve(&name)?,
            },
            format!("workspace '{name}' deleted"),
        ),
        WorkspaceOp::Rename { name, new_name } => (
            ClientRequest::RenameWorkspace {
                req_id,
                id: resolve(&name)?,
                name: new_name.clone(),
            },
            format!("workspace '{name}' renamed to '{new_name}'"),
        ),
    };
    write_frame(&mut conn.stream, &request).await?;
    await_ack(&mut conn, req_id).await?;
    println!("{done}");
    Ok(())
}

/// Ask a running daemon to shut down. Ok(false) when none is running.
///
/// A daemon on a different protocol version closes the socket right after
/// the handshake, so `Shutdown` can never reach it — exactly the situation
/// `nebula kill` exists to fix. Fall back to SIGTERM, which its handler turns
/// into the same clean shutdown, at the pid the kernel says is listening on
/// the socket — the very daemon the handshake just failed with. Only when the
/// kernel can't say does the pidfile decide, guarded by the daemon's flock so
/// a stale pid is never signalled. The pidfile can't come first: macOS's
/// tmp_cleaner deletes regular files in /tmp untouched for three days but
/// spares sockets, so a daemon that has been up that long — on any version
/// before the one that refreshes its pidfile — is still listening with no
/// pidfile beside it (#68), and a refreshing daemon re-creates the file
/// locked a moment before it writes the pid in.
pub async fn kill_daemon() -> Result<bool> {
    kill_daemon_at(&paths::socket_path(), &paths::pidfile_path()).await
}

async fn kill_daemon_at(sock: &std::path::Path, pidfile: &std::path::Path) -> Result<bool> {
    let Ok(stream) = try_connect(sock).await else {
        // Nothing listening — but a wedged or mid-boot daemon may still hold
        // the pidfile lock; fall through to the same check.
        return kill_by_pidfile(pidfile).await;
    };
    let listener_pid = peer_pid(&stream);
    if let Ok(mut conn) = handshake(stream).await {
        write_frame(&mut conn.stream, &ClientRequest::Shutdown).await?;
        wait_for_daemon_exit(pidfile, conn.daemon_pid as i32).await;
        return Ok(true);
    }
    let Some(pid) = listener_pid else {
        if kill_by_pidfile(pidfile).await? {
            return Ok(true);
        }
        bail!(
            "a nebula daemon is listening on {} but this build can't talk to it or find its \
             pid — look it up with `pgrep -f 'nebula daemon'` and stop it with `kill <pid>` \
             (never `kill -9`, which skips flushing its database)",
            sock.display()
        );
    };
    terminate(pid)?;
    wait_for_daemon_exit(pidfile, pid).await;
    Ok(true)
}

/// Outcome of `shutdown_if_idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleShutdown {
    /// Nothing listening on the socket.
    NoDaemon,
    /// The daemon held no live PTYs and was shut down cleanly.
    ShutDown,
    /// Live sessions exist; the daemon was left running.
    SessionsLive { count: usize },
    /// A daemon is listening but its protocol version differs, so its
    /// session state can't be inspected.
    Skewed,
}

/// Shut the daemon down only when it holds no live PTYs — the post-upgrade
/// handoff. An idle daemon can die safely (the next client launch spawns one
/// from the new binary on disk); live sessions would be killed with it, so
/// their daemon is left alone and the restart stays the user's call.
pub async fn shutdown_if_idle() -> Result<IdleShutdown> {
    let sock = paths::socket_path();
    let Ok(stream) = try_connect(&sock).await else {
        return Ok(IdleShutdown::NoDaemon);
    };
    let Ok(mut conn) = handshake(stream).await else {
        return Ok(IdleShutdown::Skewed);
    };
    write_frame(&mut conn.stream, &ClientRequest::Subscribe).await?;
    loop {
        match read_frame::<ServerEvent, _>(&mut conn.stream).await? {
            Some(ServerEvent::Snapshot {
                agents, terminals, ..
            }) => {
                let live = agents.iter().filter(|a| a.alive).count()
                    + terminals.iter().filter(|t| t.alive).count();
                if live > 0 {
                    return Ok(IdleShutdown::SessionsLive { count: live });
                }
                write_frame(&mut conn.stream, &ClientRequest::Shutdown).await?;
                wait_for_daemon_exit(&paths::pidfile_path(), conn.daemon_pid as i32).await;
                return Ok(IdleShutdown::ShutDown);
            }
            Some(_) => continue,
            None => bail!("daemon closed the connection before sending a snapshot"),
        }
    }
}

/// SIGTERM the daemon recorded in the pidfile (its SIGTERM handler runs the
/// same clean shutdown as `Shutdown`). Ok(false) when no daemon holds it.
async fn kill_by_pidfile(path: &std::path::Path) -> Result<bool> {
    if !daemon_holds_pidfile_lock(path) {
        return Ok(false);
    }
    let pid: i32 = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|pid| *pid > 0)
        .context("daemon is running but its pidfile is unreadable — kill it manually")?;
    terminate(pid)?;
    wait_for_daemon_exit(path, pid).await;
    Ok(true)
}

/// SIGTERM `pid`. A daemon that exits between being found and being
/// signalled — mid-shutdown under a second `nebula kill` — has done what
/// was asked, so only a process still running makes a failed signal an error.
fn terminate(pid: i32) -> Result<()> {
    if send_signal(pid, SIGTERM) != 0 && process_running(pid) {
        bail!("failed to signal daemon pid {pid} — kill it manually");
    }
    Ok(())
}

/// Liveness = flock possession (mirrors the daemon's PidfileLock): if we can
/// take the lock ourselves, nobody holds it. Released on drop.
fn daemon_holds_pidfile_lock(path: &std::path::Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    flock_try_exclusive(file.as_raw_fd()) != 0
}

/// Poll until the daemon has released its pidfile lock and exited, so a
/// relaunch right after `nebula kill` can't race the old daemon's teardown —
/// which ends by unlinking the socket path, a new daemon's socket included.
/// The process is watched as well as the lock because the lock may be on a
/// pidfile the tmp cleaner already deleted, where no client can see it.
async fn wait_for_daemon_exit(pidfile: &std::path::Path, pid: i32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while (daemon_holds_pidfile_lock(pidfile) || process_running(pid))
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(POLL_STEP).await;
    }
}

/// Whether `pid` has yet to exit. A zombie still answers signal 0, but it
/// has exited and holds nothing — it is only waiting for its parent to reap
/// it, and a daemon's parent is whichever TUI spawned it, possibly still open.
fn process_running(pid: i32) -> bool {
    if send_signal(pid, 0) != 0 {
        return false;
    }
    // Linux: the state letter follows the parenthesised command name, which
    // can itself hold spaces and parens — hence the last `)`. No `ps` needed.
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next());
        return state != Some('Z');
    }
    match std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    {
        Ok(out) => !String::from_utf8_lossy(&out.stdout)
            .trim_start()
            .starts_with('Z'),
        Err(_) => true,
    }
}

// Tiny extern shims, same dep-light idiom as nebula_core::paths.
fn flock_try_exclusive(fd: i32) -> i32 {
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    unsafe { flock(fd, LOCK_EX | LOCK_NB) }
}

const SIGTERM: i32 = 15;

fn send_signal(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relay script rides inside one pair of single quotes on the
    /// remote's login shell, so it can hold none of its own — and it has
    /// to name the fix when the remote's nebula predates `relay`.
    #[test]
    fn relay_script_survives_single_quoting_and_names_the_upgrade() {
        assert!(!RELAY_SCRIPT.contains('\''));
        assert!(!RELAY_SCRIPT.contains('\\'));
        assert!(!RELAY_SCRIPT.contains('\n'));
        assert!(RELAY_SCRIPT.ends_with("exec nebula relay"));
        assert!(RELAY_SCRIPT.contains("nebula upgrade"));
        let cmd = relay_command("https://example.com/it's.sh", &Default::default());
        assert!(cmd.starts_with("sh -c '"), "{cmd}");
        assert!(
            cmd.ends_with("nebula-relay 'https://example.com/it'\\''s.sh'"),
            "{cmd}"
        );
    }

    /// Which side is stale decides the advice, and it is about the remote
    /// machine — never `nebula kill`, which would stop the local daemon.
    #[test]
    fn remote_skew_message_points_at_the_right_machine() {
        let older = remote_skew_message("fm@box", PROTOCOL_VERSION - 1);
        assert!(older.contains("fm@box"), "{older}");
        assert!(older.contains("nebula ssh fm@box"), "{older}");
        assert!(older.contains("nebula upgrade"), "{older}");
        assert!(!older.contains("nebula kill"), "{older}");
        let newer = remote_skew_message("fm@box", PROTOCOL_VERSION + 1);
        assert!(newer.contains("upgrade nebula here"), "{newer}");
    }

    /// The binding's variables ride as positional parameters after the
    /// URL, one quoted word each, and the script exports them before
    /// nebula runs — run the way sshd runs it, with a stub `nebula` that
    /// records what it saw.
    #[test]
    fn the_relay_script_exports_the_bindings_env_before_nebula_runs() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let stub = home.path().join("stub");
        std::fs::create_dir(&stub).unwrap();
        let nebula = stub.join("nebula");
        std::fs::write(
            &nebula,
            "#!/bin/sh\nprintf '%s|%s|%s' \"$1\" \"${NEBULA_DATA_DIR-unset}\" \"${SPACED-unset}\" > \"$SEEN\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&nebula, std::fs::Permissions::from_mode(0o755)).unwrap();
        let seen = home.path().join("seen");
        let env: std::collections::BTreeMap<String, String> = [
            ("NEBULA_DATA_DIR".to_string(), "/srv/it's".to_string()),
            ("SPACED".to_string(), "a b".to_string()),
        ]
        .into();
        let cmd = relay_command("file:///nonexistent", &env);
        let status = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .env("HOME", home.path())
            .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
            .env("SEEN", &seen)
            .env_remove("NEBULA_DATA_DIR")
            .status()
            .expect("sh");
        assert!(status.success());
        // The stub answers `--help` and the real run alike; the last write
        // is the `exec nebula relay`, whose $1 is `relay`.
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            "relay|/srv/it's|a b"
        );
    }

    #[test]
    fn last_lines_keeps_the_tail_on_one_line() {
        assert_eq!(last_lines("", 3), "");
        assert_eq!(last_lines("a\n\n  b  \nc\nd\n", 3), "b · c · d");
        assert_eq!(last_lines("only", 3), "only");
    }

    // Unset and empty are the same miss, and the error has to name the
    // command the model just ran so it knows why it can't work here.
    #[test]
    fn agent_id_requires_a_non_empty_value_and_names_the_verb() {
        let err = agent_id_from(None, "rename").unwrap_err().to_string();
        assert!(err.contains("`nebula rename`"), "{err}");
        assert!(err.contains(env::AGENT_ID), "{err}");
        let err = agent_id_from(Some(String::new()), "worktree")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`nebula worktree`"), "{err}");
        assert_eq!(agent_id_from(Some("a1".into()), "rename").unwrap(), "a1");
    }

    // The whole point of the message: `nebula kill` is the fix for exactly
    // one of the two skews, and recommending it for the other sends the user
    // in a circle (kill the daemon, the live TUI respawns the same one).
    #[test]
    fn skew_message_blames_the_older_side() {
        let daemon_ahead = version_skew_message(PROTOCOL_VERSION + 2, None);
        assert!(daemon_ahead.contains("This client is the older build"));
        assert!(
            !daemon_ahead.contains("run `nebula kill` and relaunch"),
            "must not send the user to kill a daemon that is not the stale side: {daemon_ahead}"
        );
        assert!(daemon_ahead.contains("make install"));

        let daemon_behind = version_skew_message(PROTOCOL_VERSION - 1, None);
        assert!(daemon_behind.contains("The daemon is the older build"));
        assert!(daemon_behind.contains("run `nebula kill` and relaunch"));
    }

    // #68: with the daemon's pid known, the stale-daemon message names it
    // and hands over the manual fallback — SIGTERM, with -9 ruled out.
    #[test]
    fn skew_message_offers_a_clean_manual_kill_by_pid() {
        let msg = version_skew_message(PROTOCOL_VERSION - 1, Some(4242));
        assert!(msg.contains("(pid 4242)"), "{msg}");
        assert!(msg.contains("`kill 4242`"), "{msg}");
        assert!(msg.contains("never `kill -9`"), "{msg}");

        let unknown = version_skew_message(PROTOCOL_VERSION - 1, None);
        assert!(unknown.contains("the daemon:  unknown"), "{unknown}");
        assert!(!unknown.contains("`kill "), "no pid, no command: {unknown}");
    }

    /// Env var that turns [`fake_skewed_daemon`] from a no-op into a daemon.
    const FAKE_DAEMON_SOCK: &str = "NEBULA_TEST_FAKE_DAEMON_SOCK";

    /// A child re-exec of this test binary playing the daemon from #68: on
    /// an older protocol, with no pidfile, answering every `Hello` with
    /// `Incompatible` and hanging up — so only a signal stops it. A no-op
    /// when run any other way.
    #[test]
    #[ignore = "run as a child process by kill_stops_a_skewed_daemon_whose_pidfile_is_gone"]
    fn fake_skewed_daemon() {
        let Some(sock) = std::env::var_os(FAKE_DAEMON_SOCK) else {
            return;
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = tokio::net::UnixListener::bind(sock).unwrap();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _ = read_frame::<ClientRequest, _>(&mut stream).await;
                let _ = write_frame(
                    &mut stream,
                    &ServerEvent::Incompatible {
                        daemon_protocol_version: PROTOCOL_VERSION - 1,
                    },
                )
                .await;
            }
        });
    }

    /// Kills the child if the test fails before it has exited.
    struct Reap(std::process::Child);

    impl Drop for Reap {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    // #68: the pidfile is gone (macOS's tmp_cleaner) and the daemon speaks a
    // protocol this build doesn't, so neither `Shutdown` over the socket nor
    // SIGTERM via the pidfile can reach it — and `nebula kill` used to report
    // "no nebula daemon running" about a daemon still listening.
    #[tokio::test]
    async fn kill_stops_a_skewed_daemon_whose_pidfile_is_gone() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let pidfile = dir.path().join("daemon.pid");
        let mut daemon = Reap(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "ipc::tests::fake_skewed_daemon", "--ignored"])
                .env(FAKE_DAEMON_SOCK, &sock)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = daemon.0.id();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(&sock).await {
                Ok(stream) => break stream,
                Err(e) if tokio::time::Instant::now() > deadline => {
                    panic!("fake daemon never listened: {e}")
                }
                Err(_) => tokio::time::sleep(POLL_STEP).await,
            }
        };

        // What the TUI shows first: the pid, straight off the socket.
        let err = handshake(stream).await.err().expect("skewed").to_string();
        assert!(err.contains(&format!("`kill {pid}`")), "{err}");

        let started = std::time::Instant::now();
        assert!(
            kill_daemon_at(&sock, &pidfile).await.unwrap(),
            "a listening daemon is not \"no daemon running\""
        );
        // Its parent — this test — hasn't reaped it, so it is a zombie now:
        // exited, but still answering signal 0. The wait must count that as
        // gone rather than sit out its deadline.
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "kill waited {:?}",
            started.elapsed()
        );
        let status = daemon.0.try_wait().unwrap().expect("daemon exited");
        assert_eq!(status.signal(), Some(SIGTERM), "{status:?}");
    }

    #[test]
    fn skew_message_names_both_binaries() {
        let msg = version_skew_message(PROTOCOL_VERSION + 1, None);
        assert!(msg.contains("this client: "), "{msg}");
        assert!(msg.contains("the daemon:  "), "{msg}");
        // current_exe resolves under a test binary, so this half is never
        // the "unknown" fallback.
        assert!(msg.contains(&format!("v{PROTOCOL_VERSION}")), "{msg}");
    }
}
