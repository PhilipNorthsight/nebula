//! Real DAEMONs, bridge and PTYs. SSH itself is a script executing the emitted
//! remote command in a second isolated environment; no host is contacted.
use nebula_core::{
    codec::{read_frame, write_frame},
    ClientRequest, ServerEvent, PROTOCOL_VERSION,
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(15);
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Machine {
    root: PathBuf,
    repo: PathBuf,
    daemon: Daemon,
}
impl Machine {
    fn command(&self) -> Command {
        machine_command(&self.root)
    }
    fn socket(&self) -> PathBuf {
        self.root.join("r/daemon.sock")
    }
}
fn machine_command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nebula"));
    cmd.env("HOME", root.join("home"))
        .env("SHELL", "/bin/sh")
        .env("NEBULA_RUNTIME_DIR", root.join("r"))
        .env("NEBULA_DATA_DIR", root.join("d"))
        .env_remove("NEBULA_CONFIG_FILE")
        .env_remove("NEBULA_IMPORT_BUNDLE")
        .env("NEBULA_AGENT_CMD", "/bin/sh")
        .env("NEBULA_UPDATE_CHECK_SECS", "0");
    cmd
}
async fn socket(machine: &Machine) -> tokio::net::UnixStream {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(mut socket) = tokio::net::UnixStream::connect(machine.socket()).await {
                write_frame(
                    &mut socket,
                    &ClientRequest::Hello {
                        protocol_version: PROTOCOL_VERSION,
                    },
                )
                .await
                .unwrap();
                assert!(matches!(
                    read_frame::<ServerEvent, _>(&mut socket).await.unwrap(),
                    Some(ServerEvent::HelloOk { .. })
                ));
                return socket;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap()
}
async fn machine(root: PathBuf, label: &str) -> Machine {
    std::fs::create_dir_all(root.join("home/.local/bin")).unwrap();
    std::fs::write(root.join("home/.hushlogin"), "").unwrap();
    let repo = root.join(format!("{label}-repo"));
    std::fs::create_dir(&repo).unwrap();
    assert!(Command::new("/usr/bin/git")
        .args(["init", "-q", "-b", "main"])
        .arg(&repo)
        .status()
        .unwrap()
        .success());
    let daemon = Daemon(
        machine_command(&root)
            .args(["daemon", "--foreground"])
            .env("PEER_MARKER", label.to_uppercase())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let machine = Machine { root, repo, daemon };
    let mut ipc = socket(&machine).await;
    let output = machine
        .command()
        .arg("add")
        .arg(&machine.repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    write_frame(&mut ipc, &ClientRequest::Subscribe)
        .await
        .unwrap();
    let wt = match read_frame::<ServerEvent, _>(&mut ipc)
        .await
        .unwrap()
        .unwrap()
    {
        ServerEvent::Snapshot { worktrees, .. } => worktrees[0].id.clone(),
        other => panic!("{other:?}"),
    };
    write_frame(
        &mut ipc,
        &ClientRequest::CreateTerminal {
            req_id: 1,
            worktree: wt.clone(),
            name: Some("shell".into()),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            match read_frame::<ServerEvent, _>(&mut ipc)
                .await
                .unwrap()
                .unwrap()
            {
                ServerEvent::Ack { req_id: 1, .. } => break,
                ServerEvent::Error { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    if label == "remote" {
        write_frame(
            &mut ipc,
            &ClientRequest::CreateAgent {
                req_id: 2,
                worktree: wt,
                name: "stub-pi".into(),
                kind: nebula_core::AgentKind::Pi,
                model: None,
                effort: None,
                auto_title: false,
                cloud_prompt: None,
                starting_prompt: None,
                issue_url: None,
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(WAIT, async {
            loop {
                match read_frame::<ServerEvent, _>(&mut ipc)
                    .await
                    .unwrap()
                    .unwrap()
                {
                    ServerEvent::Ack { req_id: 2, .. } => break,
                    ServerEvent::Error { message, .. } => panic!("{message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
    }
    machine
}
fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
struct Screen {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Screen {
    fn send(&mut self, keys: &[u8]) {
        self.writer.write_all(keys).unwrap();
        self.writer.flush().unwrap();
    }
    fn text(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }
    fn wait(&self, text: &str) {
        let until = Instant::now() + WAIT;
        while Instant::now() < until {
            if self.text().contains(text) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("missing {text:?} in screen:\n{}", self.text());
    }
    fn capture(&self, name: &str) {
        if let Ok(dir) = std::env::var("NEBULA_TEST_CAPTURE") {
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let parser = self.parser.lock().unwrap();
            // Serialize the actual rendered cell grid. rows_formatted includes
            // cursor motion that the repository's PNG renderer doesn't interpret.
            let screen = parser.screen();
            let (height, width) = screen.size();
            let mut ansi = String::new();
            for row in 0..height {
                for col in 0..width {
                    let cell = screen.cell(row, col).unwrap();
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    ansi.push_str("\x1b[0m");
                    for (color, base) in [(cell.fgcolor(), 38), (cell.bgcolor(), 48)] {
                        match color {
                            vt100::Color::Default => {}
                            vt100::Color::Idx(n) => ansi.push_str(&format!("\x1b[{base};5;{n}m")),
                            vt100::Color::Rgb(r, g, b) => {
                                ansi.push_str(&format!("\x1b[{base};2;{r};{g};{b}m"))
                            }
                        }
                    }
                    if cell.bold() {
                        ansi.push_str("\x1b[1m");
                    }
                    if cell.inverse() {
                        ansi.push_str("\x1b[7m");
                    }
                    if cell.underline() {
                        ansi.push_str("\x1b[4m");
                    }
                    ansi.push_str(if cell.contents().is_empty() {
                        " "
                    } else {
                        cell.contents()
                    });
                }
                ansi.push('\n');
            }
            std::fs::write(dir.join(format!("{name}.ansi")), ansi).unwrap();
            std::fs::write(dir.join(format!("{name}.txt")), parser.screen().contents()).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_projects_switch_and_survive_bridge_loss_without_local_tool_access() {
    let tmp = tempfile::tempdir().unwrap();
    let mut local = machine(tmp.path().join("l"), "local").await;
    let mut remote = machine(tmp.path().join("r"), "remote").await;
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_nebula"),
        remote.root.join("home/.local/bin/nebula"),
    )
    .unwrap();
    script(&bin.join("ssh"), "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SSH_ARGS\"\nprintf '%s' \"$$\" > \"$SSH_PID\"\nexport HOME=\"$REMOTE_HOME\" NEBULA_RUNTIME_DIR=\"$REMOTE_RUNTIME\" NEBULA_DATA_DIR=\"$REMOTE_DATA\"\nfor arg do last=$arg; done\nexec /bin/sh -c \"exec $last\"\n");
    for tool in ["git", "gh", "vim", "nano"] {
        script(
            &bin.join(tool),
            "#!/bin/sh\nprintf forbidden > \"$FORBIDDEN_TOOL\"\nexit 99\n",
        );
    }
    let output = local
        .command()
        .args(["remote", "add", "fixture-box"])
        .arg(&remote.repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 36,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_nebula"));
    cmd.arg("remote");
    cmd.env("NEBULA_RUNTIME_DIR", local.root.join("r"));
    cmd.env("NEBULA_DATA_DIR", local.root.join("d"));
    cmd.env_remove("NEBULA_CONFIG_FILE");
    cmd.env_remove("NEBULA_IMPORT_BUNDLE");
    cmd.env("HOME", local.root.join("home"));
    cmd.env("TERM", "xterm-256color");
    cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    cmd.env("REMOTE_HOME", remote.root.join("home"));
    cmd.env("REMOTE_RUNTIME", remote.root.join("r"));
    cmd.env("REMOTE_DATA", remote.root.join("d"));
    cmd.env("SSH_ARGS", tmp.path().join("ssh-args"));
    cmd.env("SSH_PID", tmp.path().join("ssh-pid"));
    cmd.env("FORBIDDEN_TOOL", tmp.path().join("forbidden"));
    let child = pty.slave.spawn_command(cmd).unwrap();
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().unwrap();
    let writer = pty.master.take_writer().unwrap();
    let parser = Arc::new(Mutex::new(vt100::Parser::new(36, 160, 0)));
    let copy = parser.clone();
    std::thread::spawn(move || {
        let mut buf = [0; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            copy.lock().unwrap().process(&buf[..n]);
        }
    });
    let mut screen = Screen {
        master: pty.master,
        child,
        writer,
        parser,
    };
    screen.wait("local-repo [local]");
    screen.wait("remote-repo [SSH fixture-box]");
    screen.send(b"\r\r");
    screen.wait("typing");
    screen.send(b"printf 'OWNING_HOST=%s\\n' \"$PEER_MARKER\"\r");
    screen.wait("OWNING_HOST=LOCAL");
    screen.capture("mixed-local");
    screen.send(b"\x11j\r\r");
    screen.wait("typing");
    screen.send(b"KEEP=survived; printf 'OWNING_HOST=%s\\n' \"$PEER_MARKER\"\r");
    screen.wait("OWNING_HOST=REMOTE");
    assert!(screen.text().contains("local-repo [local]"));
    screen.capture("mixed-remote");
    // Real SIGWINCH -> TUI Resize -> remote PTY; stty reads the far-end size.
    screen
        .master
        .resize(PtySize {
            rows: 40,
            cols: 170,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    screen.parser.lock().unwrap().screen_mut().set_size(40, 170);
    std::thread::sleep(Duration::from_millis(150));
    screen.send(b"stty size\r");
    // Header=1, footer=3, borders=2 => 34 rows. Width rounding is layout-owned.
    screen.wait("34 ");
    screen.send(b"\x11gfbci");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !tmp.path().join("forbidden").exists(),
        "view invoked a local file/git/forge tool"
    );
    screen.send(b"\r\r");
    screen.wait("typing");
    let pid = std::fs::read_to_string(tmp.path().join("ssh-pid")).unwrap();
    assert!(Command::new("/bin/kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    screen.wait("OFFLINE");
    screen.wait("input blocked");
    let replay = tmp.path().join("replayed-input");
    screen.send(b"k\r\r"); // Must NOT become navigation into the local terminal.
    screen.send(format!("printf forbidden > '{}'\r", replay.display()).as_bytes());
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !replay.exists(),
        "typing during loss reached an owning shell"
    );
    screen.capture("mixed-disconnected");
    assert!(local.daemon.0.try_wait().unwrap().is_none());
    assert!(remote.daemon.0.try_wait().unwrap().is_none());
    screen.send(b"\x11k\r\r");
    screen.send(b"printf 'LOCAL_STILL=%s\\n' \"$PEER_MARKER\"\r");
    screen.wait("LOCAL_STILL=LOCAL");
    screen.send(b"\x11jR");
    screen.wait("TERMINAL · connected");
    screen.send(b"\r\r");
    screen.send(b"printf 'REMOTE_KEEP=%s\\n' \"$KEEP\"\r");
    screen.wait("REMOTE_KEEP=survived");
    screen.capture("mixed-reconnected");
    assert!(!replay.exists(), "reconnect replayed discarded input");
    // The remote agent and remote plain terminal both attach through the same view.
    screen.send(b"\x11\tj\r");
    screen.send(b"printf 'REMOTE_SHELL=%s\\n' \"$PEER_MARKER\"\r");
    screen.wait("REMOTE_SHELL=REMOTE");
    let args = std::fs::read_to_string(tmp.path().join("ssh-args")).unwrap();
    let args: Vec<_> = args.lines().collect();
    for arg in [
        "-T",
        "-a",
        "-x",
        "-oBatchMode=yes",
        "-oStrictHostKeyChecking=yes",
        "-oClearAllForwardings=yes",
    ] {
        assert!(args.contains(&arg), "{args:?}");
    }
    assert!(!args
        .iter()
        .any(|a| a.contains("NEBULA_IMPORT_BUNDLE") || a.contains("curl")));
    screen.send(b"\x11q");
    assert!(!tmp.path().join("forbidden").exists());
}
