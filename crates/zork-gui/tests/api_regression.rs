use std::time::Duration;

use bytes::Bytes;
use futures_util::{stream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zork_gui::api::{ApiError, GatewayClient, MessagePage, Role, SseStream, TranscriptMessage};

#[test]
fn gateway_history_accepts_only_delivered_user_and_assistant_messages() {
    let page: MessagePage = serde_json::from_str(
        r#"{"items":[{"type":"message","role":"user","content":"hello"},{"type":"message","role":"assistant","content":"deliberate reply"}],"older_cursor":null}"#,
    )
    .expect("gateway-delivered messages must deserialize");

    assert!(matches!(
        page.items.as_slice(),
        [
            TranscriptMessage::Message {
                role: Role::User,
                content: user
            },
            TranscriptMessage::Message {
                role: Role::Assistant,
                content: assistant
            }
        ] if user == "hello" && assistant == "deliberate reply"
    ));

    for internal in [
        r#"{"type":"message","role":"mailbox","content":"internal mailbox"}"#,
        r#"{"type":"message","role":"tool","content":"internal tool result"}"#,
        r#"{"type":"wait","reason":"internal wait"}"#,
    ] {
        assert!(
            serde_json::from_str::<TranscriptMessage>(internal).is_err(),
            "the client-visible IM schema accepted an internal Agent event: {internal}"
        );
    }
}

#[test]
fn sse_parser_preserves_utf8_split_across_transport_chunks() {
    let frame = "event: message\ndata: {\"type\":\"message\",\"role\":\"assistant\",\"content\":\"你好\"}\n\n".as_bytes();
    let split = frame
        .windows("你".len())
        .position(|window| window == "你".as_bytes())
        .expect("Chinese text is present")
        + 1;
    let chunks = vec![
        Ok::<_, ApiError>(Bytes::copy_from_slice(&frame[..split])),
        Ok(Bytes::copy_from_slice(&frame[split..])),
    ];
    let mut events = SseStream::new(stream::iter(chunks));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let event = runtime
        .block_on(events.next())
        .expect("one SSE event")
        .expect("valid SSE event");

    assert_eq!(event.name, "message");
    assert_eq!(
        event.data,
        r#"{"type":"message","role":"assistant","content":"你好"}"#
    );
}

#[test]
fn opening_sse_returns_and_delivers_data_while_connection_is_still_open() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    // GatewayClient also owns a blocking client; construct and drop it outside
    // the async runtime while exercising its streaming API inside the runtime.
    let client = GatewayClient::new(format!("http://{address}"), None);
    runtime.block_on(async {
    let (close, until_client_receives) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept SSE request");
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            socket
                .read_exact(&mut byte)
                .await
                .expect("read HTTP request");
            request.push(byte[0]);
        }
        assert!(
            String::from_utf8_lossy(&request).contains("/v1/im/sessions/live/events"),
            "GUI must subscribe to gateway-owned IM events"
        );
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: message\ndata: {\"type\":\"message\",\"role\":\"assistant\",\"content\":\"live\"}\n\n",
            )
            .await
            .expect("write SSE response");
        socket.flush().await.expect("flush SSE response");
        // Keep the response open until the client has actually received an event.
        let _ = until_client_receives.await;
    });

    let event = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = client.stream_events("live").await.expect("open SSE stream");
        events
            .next()
            .await
            .expect("one live event")
            .expect("valid SSE event")
    })
    .await
    .expect("the first event must arrive while the SSE response remains open");
    close
        .send(())
        .expect("server is still holding the connection open");
    server.await.expect("test server task");

    assert_eq!(event.name, "message");
    assert_eq!(
        event.data,
        r#"{"type":"message","role":"assistant","content":"live"}"#
    );
    });
}
