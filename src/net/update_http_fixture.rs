use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub(super) enum ResponseBody {
    Bytes(Vec<u8>),
    Chunked(Vec<Vec<u8>>),
    Truncated {
        declared_size: usize,
        bytes: Vec<u8>,
    },
    Stall,
}

#[derive(Clone, Debug)]
pub(super) struct ScriptedHttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: ResponseBody,
    delay: Duration,
}

impl ScriptedHttpResponse {
    pub(super) fn bytes(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ResponseBody::Bytes(body.into()),
            delay: Duration::ZERO,
        }
    }

    pub(super) fn chunked(status: u16, chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ResponseBody::Chunked(chunks),
            delay: Duration::ZERO,
        }
    }

    pub(super) fn truncated(status: u16, declared_size: usize, bytes: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ResponseBody::Truncated {
                declared_size,
                bytes,
            },
            delay: Duration::ZERO,
        }
    }

    pub(super) fn stalled() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: ResponseBody::Stall,
            delay: Duration::ZERO,
        }
    }

    pub(super) fn with_header(mut self, name: &str, value: impl ToString) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub(super) fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

pub(super) struct LocalHttpFixture {
    addr: SocketAddr,
    responses: Arc<Mutex<VecDeque<ScriptedHttpResponse>>>,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl LocalHttpFixture {
    pub(super) async fn start() -> Self {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = Arc::new(Mutex::new(VecDeque::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_responses = responses.clone();
        let task_requests = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let response = lock_recover(&task_responses)
                    .pop_front()
                    .unwrap_or_else(|| ScriptedHttpResponse::bytes(500, "unscripted request"));
                let requests = task_requests.clone();
                tokio::spawn(async move {
                    serve_one(stream, response, requests).await;
                });
            }
        });
        Self {
            addr,
            responses,
            requests,
            task,
        }
    }

    pub(super) fn push(&self, response: ScriptedHttpResponse) {
        lock_recover(&self.responses).push_back(response);
    }

    pub(super) fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub(super) fn requests(&self) -> Vec<String> {
        lock_recover(&self.requests).clone()
    }
}

impl Drop for LocalHttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_one(
    mut stream: TcpStream,
    response: ScriptedHttpResponse,
    requests: Arc<Mutex<Vec<String>>>,
) {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while request.len() < 16 * 1024 {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return,
            Ok(_) => request.push(byte[0]),
        }
        if request.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let request_line = String::from_utf8_lossy(&request)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    lock_recover(&requests).push(request_line);

    if !response.delay.is_zero() {
        tokio::time::sleep(response.delay).await;
    }
    if matches!(response.body, ResponseBody::Stall) {
        std::future::pending::<()>().await;
        return;
    }

    let reason = match response.status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Scripted",
    };
    let mut headers = response.headers;
    match &response.body {
        ResponseBody::Bytes(bytes) => {
            if !has_header(&headers, "content-length") {
                headers.push(("Content-Length".to_string(), bytes.len().to_string()));
            }
        }
        ResponseBody::Chunked(_) => {
            headers.push(("Transfer-Encoding".to_string(), "chunked".to_string()));
        }
        ResponseBody::Truncated { declared_size, .. } => {
            headers.push(("Content-Length".to_string(), declared_size.to_string()));
        }
        ResponseBody::Stall => unreachable!(),
    }
    headers.push(("Connection".to_string(), "close".to_string()));

    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason);
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }

    match response.body {
        ResponseBody::Bytes(bytes) => {
            let _ = stream.write_all(&bytes).await;
        }
        ResponseBody::Chunked(chunks) => {
            for chunk in chunks {
                if stream
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await
                    .is_err()
                    || stream.write_all(&chunk).await.is_err()
                    || stream.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
        }
        ResponseBody::Truncated { bytes, .. } => {
            let _ = stream.write_all(&bytes).await;
        }
        ResponseBody::Stall => unreachable!(),
    }
    let _ = stream.shutdown().await;
}

fn has_header(headers: &[(String, String)], expected: &str) -> bool {
    headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(expected))
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixture_serves_scripted_responses_and_records_requests() {
        let fixture = LocalHttpFixture::start().await;
        fixture.push(ScriptedHttpResponse::bytes(200, "fixture body"));

        let body = reqwest::get(fixture.url("/release/latest"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert_eq!(body, "fixture body");
        assert_eq!(fixture.requests(), vec!["GET /release/latest HTTP/1.1"]);
    }
}
