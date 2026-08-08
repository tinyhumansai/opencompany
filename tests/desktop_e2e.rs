//! End-to-end contract for the packaged desktop host: bundled preset → local
//! Axum runtime → loopback-only magic-link session → authenticated console API.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use opencompany::desktop::{DEFAULT_PRESET_ID, DESKTOP_OPERATOR_EMAIL, start_local};

async fn request(address: &str, request: String) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").unwrap().1
}

#[tokio::test]
async fn desktop_preset_boots_and_authenticates_the_local_operator() {
    let home = tempfile::tempdir().unwrap();
    let runtime = start_local(home.path(), DEFAULT_PRESET_ID).await.unwrap();
    let config = runtime.config();
    let address = config.api_url.strip_prefix("http://").unwrap();

    let health = request(
        address,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_string(),
    )
    .await;
    assert!(health.contains(" 200 "), "{health}");

    let login_body = format!(r#"{{"email":"{DESKTOP_OPERATOR_EMAIL}"}}"#);
    let login = request(
        address,
        format!(
            "POST /api/v1/companies/{}/auth/request HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            config.company,
            login_body.len(),
            login_body,
        ),
    )
    .await;
    assert!(login.contains(" 2"), "{login}");
    let code = serde_json::from_str::<serde_json::Value>(body(&login)).unwrap()["dev_code"]
        .as_str()
        .unwrap()
        .to_string();

    let verify_body = format!(r#"{{"code":"{code}"}}"#);
    let verified = request(
        address,
        format!(
            "POST /api/v1/companies/{}/auth/verify HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            config.company,
            verify_body.len(),
            verify_body,
        ),
    )
    .await;
    assert!(verified.starts_with("HTTP/1.1 200"));
    let cookie = verified
        .lines()
        .find_map(|line| {
            line.strip_prefix("set-cookie: ")
                .or_else(|| line.strip_prefix("Set-Cookie: "))
        })
        .unwrap()
        .split(';')
        .next()
        .unwrap();

    let status = request(
        address,
        format!(
            "GET /api/v1/companies/{} HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n",
            config.company,
        ),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}
