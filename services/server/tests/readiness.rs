use std::time::Duration;

use peppy_server::{config::S3Config, storage::Storage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

#[tokio::test]
async fn storage_initialization_provisions_the_configured_bucket() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 1024];
            let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
                .await
                .unwrap()
                .unwrap();
            assert!(read > 0, "request ended before its headers");
            request.extend_from_slice(&chunk[..read]);
        }
        let request = std::str::from_utf8(&request).unwrap();
        assert!(request.starts_with("PUT /fresh-private-bucket HTTP/1.1\r\n"));
        assert!(request.contains("\r\nauthorization: AWS4-HMAC-SHA256 "));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });

    let config = S3Config {
        endpoint: Url::parse(&format!("http://{address}")).unwrap(),
        bucket: "fresh-private-bucket".into(),
        access_key: "testaccess".into(),
        secret_key: "testsecret".into(),
        readiness_timeout: Duration::from_secs(2),
    };

    Storage::initialize(&config).await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn storage_initialization_fails_closed_when_provisioning_is_unreachable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let config = S3Config {
        endpoint: Url::parse(&format!("http://{address}")).unwrap(),
        bucket: "fresh-private-bucket".into(),
        access_key: "testaccess".into(),
        secret_key: "testsecret".into(),
        readiness_timeout: Duration::from_secs(2),
    };

    let error = match Storage::initialize(&config).await {
        Ok(_) => panic!("storage initialization unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "storage unavailable: S3 bucket request failed"
    );
}
