//! 测试用的本机 HTTP 服务：记下每个请求，按闭包给响应。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
}

impl Request {
    pub fn cookie(&self) -> Option<&str> {
        self.headers.get("cookie").map(String::as_str)
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: body.into(),
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location", location.into())],
            body: Vec::new(),
        }
    }

    pub fn bytes(body: &[u8]) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type", "application/octet-stream".into())],
            body: body.to_vec(),
        }
    }

    pub fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

pub struct Server {
    pub base: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Server {
    pub fn start(handler: impl Fn(&Request, &str) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let log = requests.clone();
        let base_for_handler = base.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let handler = handler.clone();
                let log = log.clone();
                let base = base_for_handler.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let target = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let mut headers = BTreeMap::new();
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                            break;
                        }
                        if let Some((k, v)) = header.trim_end().split_once(':') {
                            headers.insert(k.to_ascii_lowercase(), v.trim().to_owned());
                        }
                    }
                    let parsed = url::Url::parse(&format!("http://x{target}")).unwrap();
                    let request = Request {
                        path: parsed.path().to_owned(),
                        query: parsed
                            .query_pairs()
                            .map(|(k, v)| (k.into(), v.into()))
                            .collect(),
                        headers,
                    };
                    log.lock().unwrap().push(request.clone());
                    let response = handler(&request, &base);
                    let mut head =
                        format!("HTTP/1.1 {} X\r\nConnection: close\r\n", response.status);
                    // 带 `X-No-Length` 的响应不写 Content-Length，靠关闭连接结束
                    if !response.headers.iter().any(|(k, _)| *k == "X-No-Length") {
                        head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
                    }
                    for (k, v) in response.headers.iter().filter(|(k, _)| *k != "X-No-Length") {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    stream.write_all(head.as_bytes()).ok();
                    stream.write_all(&response.body).ok();
                });
            }
        });
        Self { base, requests }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.path).collect()
    }
}

/// 读 `tests/fixtures/<name>`，把 `__BASE__` 换成服务器地址。
pub fn fixture(name: &str, base: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(path)
        .unwrap()
        .replace("__BASE__", base)
}
