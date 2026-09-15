//! Public remote catalogue and byte-transparent bridge contracts; no SSH server.
use nebula_core::{
    codec::{read_frame, write_frame},
    ClientRequest, ServerEvent, PROTOCOL_VERSION,
};
use std::{
    path::Path,
    process::{Command, Stdio},
};

fn cli(data: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nebula"));
    cmd.env("NEBULA_DATA_DIR", data)
        .env_remove("NEBULA_CONFIG_FILE")
        .env_remove("NEBULA_IMPORT_BUNDLE");
    cmd
}

#[test]
fn remote_registration_is_local_metadata_not_a_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path();
    let path = "/nonexistent/it's a remote repo";
    for _ in 0..2 {
        let out = cli(data)
            .args(["remote", "add", "fixture-box", path])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = cli(data).args(["remote", "list"]).output().unwrap();
    assert!(out.status.success());
    let entries: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 1);
    assert_eq!(entries[0]["host"], "fixture-box");
    assert_eq!(entries[0]["path"], path);
    assert!(!data.join("nebula.db").exists());
    assert!(!data.join("config.json").exists());
    let out = cli(data)
        .args(["remote", "remove", "fixture-box", path])
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = cli(data).args(["remote", "list"]).output().unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::json!([])
    );
}

#[test]
fn unsafe_destinations_and_relative_remote_paths_are_rejected() {
    let data = tempfile::tempdir().unwrap();
    for (host, path) in [
        ("-oProxyCommand=touch", "/repo"),
        ("box\ncommand", "/repo"),
        ("box name", "/repo"),
        ("box", "relative"),
        ("box", "/repo\nother"),
    ] {
        let out = cli(data.path())
            .args(["remote", "add", "--", host, path])
            .output()
            .unwrap();
        assert!(!out.status.success(), "accepted {host:?} {path:?}");
    }
    assert!(!data.path().join("remote_projects.json").exists());
}

#[tokio::test]
async fn stdio_bridge_carries_real_frames_without_importing_settings_or_spawning_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let bundle = nebula_tui::bundle::encode(
        &serde_json::json!({"nebula_bundle":1,"config":{"editor":"do-not-import"}}),
    );
    let mut cmd = tokio::process::Command::from(cli(tmp.path()));
    let mut child = cmd
        .arg("_stdio")
        .env("NEBULA_RUNTIME_DIR", tmp.path())
        .env("NEBULA_IMPORT_BUNDLE", bundle)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        write_frame(
            &mut input,
            &ClientRequest::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<ClientRequest, _>(&mut socket).await.unwrap(),
            Some(ClientRequest::Hello {
                protocol_version: PROTOCOL_VERSION
            })
        ));
        write_frame(
            &mut socket,
            &ServerEvent::HelloOk {
                protocol_version: PROTOCOL_VERSION,
                daemon_pid: 123,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<ServerEvent, _>(&mut output).await.unwrap(),
            Some(ServerEvent::HelloOk {
                daemon_pid: 123,
                ..
            })
        ));
        drop(input);
        assert!(read_frame::<ClientRequest, _>(&mut socket)
            .await
            .unwrap()
            .is_none());
        drop(socket);
        assert!(child.wait().await.unwrap().success());
    })
    .await
    .expect("bridge hung or failed to connect");
    assert!(!tmp.path().join("config.json").exists());
    assert!(!tmp.path().join("nebula.db").exists());
}
