use super::*;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn outgoing_requests_do_not_cancel_a_partially_received_frame() {
    let (client, mut daemon) = tokio::io::duplex(4096);
    let (reader, writer) = tokio::io::split(client);
    let (requests, rx) = mpsc::channel(8);
    let (events, mut incoming) = mpsc::channel(8);
    let task = tokio::spawn(async move { exchange(reader, writer, rx, &events, 1, 7).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        assert!(matches!(
            read_frame::<ClientRequest, _>(&mut daemon).await.unwrap(),
            Some(ClientRequest::Hello {
                protocol_version: PROTOCOL_VERSION
            })
        ));
        write_frame(
            &mut daemon,
            &ServerEvent::HelloOk {
                protocol_version: PROTOCOL_VERSION,
                daemon_pid: 42,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<ClientRequest, _>(&mut daemon).await.unwrap(),
            Some(ClientRequest::Subscribe)
        ));
        let session = nebula_core::SessionRef::Terminal("fixture".to_owned().into());
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
            &ServerEvent::Output {
                session: session.clone(),
                seq: 0,
                data: b"complete".to_vec(),
            },
        )
        .await
        .unwrap();
        daemon.write_all(&frame[..2]).await.unwrap();
        tokio::task::yield_now().await;
        requests
            .send(ClientRequest::Resize {
                session,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<ClientRequest, _>(&mut daemon).await.unwrap(),
            Some(ClientRequest::Resize {
                cols: 80,
                rows: 24,
                ..
            })
        ));
        daemon.write_all(&frame[2..]).await.unwrap();
        let message = incoming.recv().await.unwrap();
        assert_eq!((message.peer, message.generation), (1, 7));
        assert!(matches!(message.event, Ok(ServerEvent::Output {data, ..}) if data == b"complete"));
        drop(requests);
        assert!(task.await.unwrap().is_ok());
    })
    .await
    .expect("partial frame lost or deadlock");
}

#[tokio::test]
async fn protocol_mismatch_closes_only_this_connection_without_subscribing_or_shutdown() {
    let (client, mut daemon) = tokio::io::duplex(4096);
    let (reader, writer) = tokio::io::split(client);
    let (_requests, rx) = mpsc::channel(8);
    let (events, _incoming) = mpsc::channel(8);
    let task = tokio::spawn(async move { exchange(reader, writer, rx, &events, 1, 0).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        assert!(matches!(
            read_frame::<ClientRequest, _>(&mut daemon).await.unwrap(),
            Some(ClientRequest::Hello { .. })
        ));
        write_frame(
            &mut daemon,
            &ServerEvent::Incompatible {
                daemon_protocol_version: PROTOCOL_VERSION + 1,
            },
        )
        .await
        .unwrap();
        let err = task.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("no daemon was restarted"));
        assert!(
            read_frame::<ClientRequest, _>(&mut daemon)
                .await
                .unwrap()
                .is_none(),
            "must send neither Subscribe nor Shutdown"
        );
    })
    .await
    .unwrap();
}
