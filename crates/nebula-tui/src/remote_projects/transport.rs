//! A connection owns only a socket (local) or an SSH child (remote), never a DAEMON.
use anyhow::{bail, Context, Result};
use nebula_core::{
    codec::{read_frame, write_frame},
    ClientRequest, ServerEvent, PROTOCOL_VERSION,
};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
    task::JoinHandle,
};

#[cfg(test)]
mod tests;

const TIMEOUT: Duration = Duration::from_secs(6);
pub struct Message {
    pub peer: usize,
    pub generation: u64,
    pub event: Result<ServerEvent, String>,
}

pub struct Connection {
    pub tx: mpsc::Sender<ClientRequest>,
    task: JoinHandle<()>,
}
#[cfg(test)]
impl Connection {
    pub(super) fn fixture() -> (Self, mpsc::Receiver<ClientRequest>) {
        let (tx, rx) = mpsc::channel(32);
        (
            Self {
                tx,
                task: tokio::spawn(std::future::pending()),
            },
            rx,
        )
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The only shell text is fixed. No settings, paths, credentials or installer travel.
const BRIDGE_COMMAND: &str = "sh -c 'PATH=\"$HOME/.local/bin:$PATH:/opt/homebrew/bin:/usr/local/bin\"; export PATH; exec nebula _stdio'";

pub fn ssh_command(host: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args([
        "-T",
        "-a",
        "-x",
        "-oBatchMode=yes",
        "-oStrictHostKeyChecking=yes",
        "-oClearAllForwardings=yes",
        "-oConnectTimeout=5",
        "-oServerAliveInterval=5",
        "-oServerAliveCountMax=2",
        "--",
        host,
        BRIDGE_COMMAND,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    // Authentication never prompts over the TUI, nor can SSH diagnostics corrupt it.
    .stderr(Stdio::null())
    .kill_on_drop(true);
    cmd
}

pub fn connect(
    peer: usize,
    generation: u64,
    host: Option<String>,
    socket: PathBuf,
    events: mpsc::Sender<Message>,
) -> Connection {
    let (tx, rx) = mpsc::channel(32);
    let task = tokio::spawn(async move {
        let result = async {
            if let Some(host) = host {
                let mut child = ssh_command(&host).spawn().context("start OpenSSH")?;
                let reader = child.stdout.take().context("SSH stdout")?;
                let writer = child.stdin.take().context("SSH stdin")?;
                let result = exchange(reader, writer, rx, &events, peer, generation).await;
                // EOF/detach never sends Shutdown. Reap only this bridge connection.
                let _ = child.kill().await;
                let _ = child.wait().await;
                result
            } else {
                let stream = tokio::net::UnixStream::connect(socket)
                    .await
                    .context("connect to local daemon; start normal nebula first")?;
                let (reader, writer) = stream.into_split();
                exchange(reader, writer, rx, &events, peer, generation).await
            }
        }
        .await;
        if let Err(err) = result {
            let _ = events.send(Message {peer, generation, event: Err(format!("{err:#}; check SSH trust, matching binary and running daemon; R reconnects"))}).await;
        }
    });
    Connection { tx, task }
}

async fn exchange<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    mut requests: mpsc::Receiver<ClientRequest>,
    events: &mpsc::Sender<Message>,
    peer: usize,
    generation: u64,
) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        write_frame(&mut writer, &ClientRequest::Hello { protocol_version: PROTOCOL_VERSION }).await?;
        match read_frame::<ServerEvent,_>(&mut reader).await? {
            Some(ServerEvent::HelloOk { protocol_version: PROTOCOL_VERSION, .. }) => {},
            Some(ServerEvent::Incompatible { daemon_protocol_version }) => bail!("daemon protocol {daemon_protocol_version}, expected {PROTOCOL_VERSION}; no daemon was restarted"),
            _ => bail!("bridge closed or incompatible handshake"),
        }
        write_frame(&mut writer, &ClientRequest::Subscribe).await?;
        Ok::<_,anyhow::Error>(())
    }).await.context("handshake timed out")??;
    // The reader must retain its partial frame across outgoing requests: cancelling
    // read_frame in a select loop would discard an in-flight length/payload prefix.
    let receive = async {
        while let Some(event) = read_frame::<ServerEvent, _>(&mut reader).await? {
            events
                .send(Message {
                    peer,
                    generation,
                    event: Ok(event),
                })
                .await
                .map_err(|_| anyhow::anyhow!("view closed"))?;
        }
        bail!("connection closed")
    };
    let transmit = async {
        while let Some(req) = requests.recv().await {
            tokio::time::timeout(TIMEOUT, write_frame(&mut writer, &req))
                .await
                .context("write timed out")??;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! { result = receive => result, result = transmit => result }
}

/// Byte-transparent, connect-only entry point for `nebula _stdio`. Using blocking
/// stdio here avoids a Tokio stdin blocking task keeping a disconnected helper alive.
pub fn bridge() -> Result<()> {
    use std::net::Shutdown;
    let mut read = std::os::unix::net::UnixStream::connect(nebula_core::paths::socket_path())
        .context("remote daemon is not running; start nebula on that machine first")?;
    let mut write = read.try_clone()?;
    std::thread::spawn(move || {
        let _ = copy_bytes(&mut std::io::stdin().lock(), &mut write);
        let _ = write.shutdown(Shutdown::Write);
    });
    copy_bytes(&mut read, &mut std::io::stdout().lock())?;
    Ok(())
}

// Explicit buffered copies: stdout is line-buffered, but binary frames have
// no newline. Flush each chunk; do not use io::copy's socket/pipe splice path.
fn copy_bytes(
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let mut bytes = [0; 16 * 1024];
    loop {
        let n = reader.read(&mut bytes)?;
        if n == 0 {
            return Ok(());
        }
        writer.write_all(&bytes[..n])?;
        writer.flush()?;
    }
}
