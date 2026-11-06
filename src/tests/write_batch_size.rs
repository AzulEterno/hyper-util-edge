//! Verify auto-server -> Hyper -> h2 forwarding through actual response bytes.
use crate::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Empty, Full};
use hyper::service::service_fn;
use std::{
    convert::Infallible,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::AsyncWrite;

struct Wire {
    inner: tokio::io::DuplexStream,
    vectored: bool,
    writes: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}
impl tokio::io::AsyncRead for Wire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl AsyncWrite for Wire {
    fn is_write_vectored(&self) -> bool {
        self.vectored
    }
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                if n > 0 {
                    self.writes.lock().unwrap().push(buf[..n].to_vec());
                }
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let flat: Vec<u8> = bufs.iter().flat_map(|s| s.iter().copied()).collect();
        self.poll_write(cx, &flat)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn response_writes(target: Option<usize>, vectored: bool) -> usize {
    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let server_io = Wire {
        inner: server_io,
        vectored,
        writes: writes.clone(),
    };
    let mut builder = Builder::new(TokioExecutor::new());
    builder.http2().write_batch_size(target).unwrap();
    for invalid in [0, 32785, usize::MAX] {
        assert_eq!(
            builder
                .http2()
                .write_batch_size(Some(invalid))
                .err()
                .expect("invalid target must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    let server = tokio::spawn(async move {
        builder
            .serve_connection(
                TokioIo::new(server_io),
                service_fn(|_| async {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(vec![7; 9 * 16384]))))
                }),
            )
            .await
            .unwrap();
    });
    let mut client_builder = hyper::client::conn::http2::Builder::new(TokioExecutor::new());
    client_builder
        .initial_stream_window_size(1024 * 1024)
        .initial_connection_window_size(1024 * 1024);
    let (mut sender, connection) = client_builder
        .handshake(TokioIo::new(client_io))
        .await
        .unwrap();
    let driver = tokio::spawn(connection);
    let response = sender
        .send_request(
            Request::builder()
                .uri("https://example.com/")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), vec![7; 9 * 16384]);
    drop(sender);
    driver.await.unwrap().unwrap();
    server.await.unwrap();
    let writes = writes.lock().unwrap();
    let bytes: Vec<u8> = writes.iter().flatten().copied().collect();
    let mut offset = 0;
    let mut data_spans = Vec::new();
    while offset < bytes.len() {
        let len = ((bytes[offset] as usize) << 16)
            | ((bytes[offset + 1] as usize) << 8)
            | bytes[offset + 2] as usize;
        let end = offset + 9 + len;
        if bytes[offset + 3] == 0 && len > 0 {
            data_spans.push((offset, end));
        }
        offset = end;
    }
    assert_eq!(data_spans.len(), 9);
    let mut offset = 0;
    writes
        .iter()
        .filter(|write| {
            let start = offset;
            offset += write.len();
            data_spans.iter().any(|&(a, b)| a < offset && b > start)
        })
        .count()
}
#[tokio::test]
async fn write_batch_size_forwards_to_response_encoding() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for vectored in [false, true] {
            let disabled = response_writes(None, vectored).await;
            let enabled = response_writes(Some(131144), vectored).await;
            assert_eq!(enabled, 2);
            assert!(disabled >= 9, "disabled={disabled}, vectored={vectored}");
        }
    })
    .await
    .expect("response forwarding test timed out");
}
