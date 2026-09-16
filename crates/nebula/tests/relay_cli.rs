//! `nebula relay`: the far end of a REMOTE WORKSPACE. Spawned the way ssh
//! would spawn it — a bare process with the daemon protocol on its stdio —
//! it must bring up this machine's daemon when none is listening and carry
//! the client's frames to it unchanged, so the handshake and the first
//! Snapshot come back exactly as they would over the local socket.

use nebula_core::codec::{read_frame, write_frame};
use nebula_core::{env, ClientRequest, ServerEvent, PROTOCOL_VERSION};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Isolated runtime + data dirs, cleaned up with the daemon they held.
struct Instance {
    runtime_dir: PathBuf,
    data_dir: PathBuf,
}

impl Instance {
    fn new() -> Self {
        let pid = std::process::id();
        // Socket paths must stay under SUN_LEN; keep the runtime dir short.
        let runtime_dir = PathBuf::from(format!("/tmp/nebrelay-rt-{pid}"));
        let data_dir = PathBuf::from(format!("/tmp/nebrelay-data-{pid}"));
        let _ = std::fs::remove_dir_all(&runtime_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
        Self {
            runtime_dir,
            data_dir,
        }
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_nebula"));
        cmd.args(args)
            .env(env::RUNTIME_DIR, &self.runtime_dir)
            .env(env::DATA_DIR, &self.data_dir)
            .env(env::AGENT_CMD, "/bin/sh")
            .env(env::UPDATE_CHECK_SECS, "0");
        cmd
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_nebula"))
            .arg("kill")
            .env(env::RUNTIME_DIR, &self.runtime_dir)
            .env(env::DATA_DIR, &self.data_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

#[tokio::test]
async fn relay_brings_up_the_daemon_and_carries_the_protocol_end_to_end() {
    let instance = Instance::new();
    // Nothing is listening yet: the relay itself has to spawn the daemon,
    // exactly as a TUI launched on that machine would.
    assert!(!instance.runtime_dir.join("daemon.sock").exists());

    let mut relay = instance
        .command(&["relay"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn nebula relay");
    let mut stdin = relay.stdin.take().unwrap();
    let mut stdout = tokio::io::BufReader::new(relay.stdout.take().unwrap());

    let run = async {
        write_frame(
            &mut stdin,
            &ClientRequest::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        match read_frame::<ServerEvent, _>(&mut stdout).await.unwrap() {
            Some(ServerEvent::HelloOk {
                protocol_version,
                daemon_pid,
            }) => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_ne!(daemon_pid, 0);
            }
            other => panic!("expected HelloOk, got {other:?}"),
        }
        write_frame(&mut stdin, &ClientRequest::Subscribe)
            .await
            .unwrap();
        loop {
            match read_frame::<ServerEvent, _>(&mut stdout).await.unwrap() {
                Some(ServerEvent::Snapshot { workspaces, .. }) => {
                    // A fresh daemon has exactly the built-in workspace.
                    assert_eq!(workspaces.len(), 1);
                    assert_eq!(workspaces[0].name, "default");
                    break;
                }
                Some(_) => continue,
                None => panic!("relay closed before the snapshot"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("handshake and snapshot within 20s");

    // Hanging up the client end ends the relay — and only the relay: the
    // daemon it started is still there for the next connection.
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(10), relay.wait())
        .await
        .expect("relay exits once its stdin closes")
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert!(instance.runtime_dir.join("daemon.sock").exists());
}
