//! B站的 HTTP 接口：只调不需要签名的接口。cookie 只发给 API 主机；
//! 展开短链、下封面和视频流的请求一律不带 cookie。网络上来的每个字节都不可信。
//! 请求都经 `usuyo_plugin_kit::http`：每种请求只连自己那几台主机，只连公网地址，都不跟跳转。

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;
use usuyo_plugin_kit::http::{self, Client, Cookie, Hosts, Policy};
use usuyo_plugin_kit::protocol::{CallFailure, ErrorCode, NotFoundReason, PluginEvent};

use crate::ids::is_video_host;
use crate::session::Session;

/// 清单 `sites` 里的站点，错误里的 `site` 也写它。
pub const SITE: &str = "www.bilibili.com";

/// 普通桌面浏览器的 UA。
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
const REFERER: &str = "https://www.bilibili.com/";
/// API 响应上限。
const API_BYTES: u64 = 8 << 20;
const API_TIMEOUT: Duration = Duration::from_secs(20);
/// 整次调用的总时限，比清单里的 `timeout: 30m` 短，留出交回已取到部分的时间。
const CALL_BUDGET: Duration = Duration::from_secs(28 * 60);
const SHORT_LINK_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_SHORT_LINK_HOPS: usize = 4;
/// 封面、字幕、视频流只从这些域名的子域下载（和清单的 `network.hosts` 一致）。
const MEDIA_HOST_SUFFIXES: [&str; 3] = [".bilivideo.com", ".bilivideo.cn", ".hdslb.com"];
const SHORT_LINK_HOST: &str = "b23.tv";
/// 错误信息里照抄服务器 `message` 的长度上限。
const MESSAGE_CHARS: usize = 120;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("network: {0}")]
    Network(String),
    #[error("HTTP {status}")]
    Http {
        status: u16,
        retry_after: Option<u64>,
    },
    #[error("bilibili returned code {code}: {message}")]
    Code { code: i64, message: String },
    #[error("unexpected response: {0}")]
    Shape(String),
    #[error("a response is larger than {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("the host `{0}` is not one of the allowed media hosts")]
    ForbiddenHost(String),
    /// 连之前的检查没过：主机不在这种请求的名单里，或地址不是公网。
    #[error("refused to connect: {0}")]
    Refused(String),
    #[error("{0}")]
    Credentials(String),
    /// 整次调用的时限到了。可选的那一步遇到它只记进 `missing`，已经取到的照样交出去。
    #[error("the call's time budget is used up")]
    OutOfTime,
    #[error("cannot write `{path}`: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    /// 接口回了，但不是能用的回答：带着回答的开头，宿主记进失败日志。
    #[error("{error}")]
    Answered {
        error: Box<ApiError>,
        answer: String,
    },
}

/// 留给失败日志的回答开头最多多少字节。
const ANSWER_BYTES: u64 = 4096;

impl ApiError {
    /// 需要停下来报错的：网络、风控、限流、登录过期。其余的（找不到、格式变了、太大）
    /// 在可选的那一步里只记进 `missing`，主体照样交出去。
    pub fn is_hard(&self) -> bool {
        match self {
            ApiError::Network(_) | ApiError::Credentials(_) | ApiError::Io { .. } => true,
            ApiError::Http { status, .. } => !matches!(status, 404 | 410),
            ApiError::Code { code, .. } => matches!(code, -412 | -352 | -101 | -509 | -799),
            ApiError::Shape(_)
            | ApiError::TooLarge { .. }
            | ApiError::ForbiddenHost(_)
            | ApiError::Refused(_)
            | ApiError::OutOfTime => false,
            ApiError::Answered { error, .. } => error.is_hard(),
        }
    }

    /// 接口回的 `code`（不是 0 时）。
    pub fn code(&self) -> Option<i64> {
        match self {
            ApiError::Code { code, .. } => Some(*code),
            ApiError::Answered { error, .. } => error.code(),
            _ => None,
        }
    }

    fn answered(self, raw: &[u8]) -> ApiError {
        let end = raw.len().min(ANSWER_BYTES as usize);
        ApiError::Answered {
            error: Box::new(self),
            answer: String::from_utf8_lossy(&raw[..end]).into_owned(),
        }
    }
}

impl From<ApiError> for CallFailure {
    fn from(err: ApiError) -> CallFailure {
        failure(&err)
    }
}

fn failure(err: &ApiError) -> CallFailure {
    let message = err.to_string();
    let mut failure = match err {
        ApiError::Network(_) | ApiError::OutOfTime => CallFailure::new(ErrorCode::Network, message),
        ApiError::Http {
            status,
            retry_after,
        } => match status {
            412 | 403 => blocked(message),
            401 => login_required(message),
            404 => CallFailure::new(ErrorCode::NotFound, message),
            410 => not_found(message, NotFoundReason::Deleted),
            429 => CallFailure {
                retry_after: *retry_after,
                ..CallFailure::new(ErrorCode::RateLimited, message)
            },
            500..=599 => CallFailure::new(ErrorCode::Network, message),
            _ => CallFailure::new(ErrorCode::UpstreamChanged, message),
        },
        ApiError::Code { code, .. } => match code {
            -412 | -352 => blocked(message),
            -101 => login_required(message),
            -404 => not_found(message, NotFoundReason::Deleted),
            -403 | 62002 | 62012 => not_found(message, NotFoundReason::Private),
            62004 | -400 => CallFailure::new(ErrorCode::NotFound, message),
            -509 | -799 => CallFailure::new(ErrorCode::RateLimited, message),
            _ => CallFailure::new(ErrorCode::UpstreamChanged, message),
        },
        ApiError::Shape(_)
        | ApiError::TooLarge { .. }
        | ApiError::ForbiddenHost(_)
        | ApiError::Refused(_) => CallFailure::new(ErrorCode::UpstreamChanged, message),
        ApiError::Credentials(_) | ApiError::Io { .. } => {
            CallFailure::new(ErrorCode::Internal, message)
        }
        ApiError::Answered { error, answer } => return failure(error).with_detail(answer),
    };
    if failure.code == ErrorCode::RateLimited {
        failure.hint = Some("B站提示请求太频繁，请过一会儿再试".into());
    }
    failure
}

fn blocked(message: String) -> CallFailure {
    CallFailure {
        hint: Some(
            "B站出现了风控提示；请稍后再试，也可以先在浏览器里打开 B站确认账号一切正常".into(),
        ),
        site: Some(SITE.into()),
        ..CallFailure::new(ErrorCode::Blocked, message)
    }
}

fn login_required(message: String) -> CallFailure {
    CallFailure {
        hint: Some("B站的登录已过期，请在设置里重新登录 B站".into()),
        site: Some(SITE.into()),
        ..CallFailure::new(ErrorCode::LoginRequired, message)
    }
}

fn not_found(message: String, reason: NotFoundReason) -> CallFailure {
    CallFailure {
        reason: Some(reason),
        ..CallFailure::new(ErrorCode::NotFound, message)
    }
}

/// 要连的主机和地址策略。正式运行用 `production`；测试把它指到本机的假服务器。
#[derive(Debug, Clone)]
pub struct Endpoints {
    api_base: Url,
    /// 测试用的本机主机：算短链主机，也算允许的媒体主机，媒体链接不升级成 https。
    local_host: Option<String>,
    policy: Policy,
}

impl Endpoints {
    pub fn production() -> Self {
        Self {
            api_base: Url::parse("https://api.bilibili.com/").expect("constant URL"),
            local_host: None,
            policy: Policy::from_host(),
        }
    }

    /// 测试用：`base` 是 `http://127.0.0.1:<端口>`，`policy` 让这个本机地址算公网。
    pub fn local(base: &str, policy: Policy) -> Self {
        let api_base = Url::parse(base).expect("local base URL");
        let local_host = api_base.host_str().map(str::to_owned);
        Self {
            api_base,
            local_host,
            policy,
        }
    }

    pub fn is_short_link_host(&self, host: &str) -> bool {
        host == SHORT_LINK_HOST || self.local_host.as_deref() == Some(host)
    }

    fn is_media_host(&self, host: &str) -> bool {
        self.local_host.as_deref() == Some(host)
            || MEDIA_HOST_SUFFIXES
                .iter()
                .any(|s| host.len() > s.len() && host.ends_with(s))
    }

    fn api_host(&self) -> &str {
        self.api_base.host_str().unwrap_or_default()
    }

    fn client(&self, hosts: &[&str]) -> Client {
        let local = self.local_host.as_deref();
        Client::new(USER_AGENT, Hosts::only(hosts.iter().copied().chain(local)))
            .with_policy(self.policy)
    }

    /// 接口只连 API 主机。
    fn api_client(&self) -> Client {
        self.client(&[self.api_host()])
    }

    fn short_link_client(&self) -> Client {
        self.client(&[SHORT_LINK_HOST])
    }

    fn media_client(&self) -> Client {
        self.client(&MEDIA_HOST_SUFFIXES)
    }

    /// 接口给的媒体地址：补 scheme、升级成 https、核对主机。
    pub fn media_url(&self, raw: &str) -> Result<Url, ApiError> {
        let raw = raw.trim();
        let full = if raw.starts_with("//") {
            format!("https:{raw}")
        } else {
            raw.to_owned()
        };
        let mut url = Url::parse(&full)
            .map_err(|_| ApiError::Shape("a media address is not a URL".into()))?;
        let host = url
            .host_str()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !self.is_media_host(&host) {
            return Err(ApiError::ForbiddenHost(host));
        }
        let local = self.local_host.as_deref() == Some(host.as_str());
        // 清单的 `network.hosts` 没写端口，只放行默认端口；别的端口宿主的代理会拒，
        // 这里先当不可用，调用方换备用地址
        if !local && url.port().is_some() {
            return Err(ApiError::ForbiddenHost(format!(
                "{host}:{}",
                url.port().unwrap_or_default()
            )));
        }
        if url.scheme() == "http" && !local {
            url.set_scheme("https")
                .map_err(|()| ApiError::Shape("a media address cannot use https".into()))?;
        }
        if url.scheme() != "https" && !local {
            return Err(ApiError::Shape("a media address is not http(s)".into()));
        }
        Ok(url)
    }
}

/// 一次 API 调用的结果：`code` 是 0 才会有。
pub struct Reply {
    /// `data` 字段。
    pub data: Value,
    /// 原始响应，原样存进输出目录。
    pub raw: Vec<u8>,
}

impl Reply {
    pub fn data<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        T::deserialize(&self.data).map_err(|e| ApiError::Shape(format!("data: {e}")))
    }
}

#[derive(Deserialize)]
struct Wire {
    code: Option<i64>,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Value,
}

// 请求都不跟随重定向：接口和 CDN 都不该重定向，短链的跳转自己一跳一跳读
fn transport(e: http::Error) -> ApiError {
    match e {
        http::Error::TooLarge(limit) => ApiError::TooLarge { limit },
        e if e.is_refusal() => ApiError::Refused(e.to_string()),
        e => ApiError::Network(e.to_string()),
    }
}

fn check_status(response: &http::Response) -> Result<(), ApiError> {
    let status = response.status();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let retry_after = response
        .header("retry-after")
        .and_then(|v| v.trim().parse().ok());
    Err(ApiError::Http {
        status,
        retry_after,
    })
}

fn read_limited(response: &mut http::Response, limit: u64) -> Result<Vec<u8>, ApiError> {
    response.read_to_vec(limit).map_err(transport)
}

pub struct Bilibili {
    endpoints: Endpoints,
    session: Option<Session>,
    /// 整次调用的时限：每一步只能用到这里为止。
    deadline: Instant,
}

impl Bilibili {
    pub fn new(endpoints: Endpoints, session: Option<Session>) -> Self {
        Self {
            endpoints,
            session,
            deadline: Instant::now() + CALL_BUDGET,
        }
    }

    /// 测试用：换一个短的总时限。
    pub fn with_budget(self, budget: Duration) -> Self {
        Self {
            deadline: Instant::now() + budget,
            ..self
        }
    }

    /// 一步能用多久：它自己的上限和整次调用剩下的，取短的。剩不到一秒就不发了。
    fn step_timeout(&self, step: Duration) -> Result<Duration, ApiError> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_secs(1) {
            return Err(ApiError::OutOfTime);
        }
        Ok(step.min(left))
    }

    /// 时限一过，这一步的网络错误多半是被总时限截断的，按时限到了算。
    fn out_of_time(&self, err: ApiError) -> ApiError {
        match err {
            ApiError::Network(_) if Instant::now() >= self.deadline => ApiError::OutOfTime,
            other => other,
        }
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// 调一个 API 接口。这是唯一带 cookie 的请求。
    pub fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Reply, ApiError> {
        let mut url = self
            .endpoints
            .api_base
            .join(path)
            .map_err(|e| ApiError::Shape(format!("bad endpoint `{path}`: {e}")))?;
        url.query_pairs_mut().extend_pairs(query);
        let client = self.endpoints.api_client();
        let mut request = client
            .get(&url)
            .header("Referer", REFERER)
            .header("Accept", "application/json")
            .timeout(self.step_timeout(API_TIMEOUT)?);
        if let Some(session) = &self.session {
            let cookie = Cookie::new(session.cookie_header(), [self.endpoints.api_host()]);
            request = request.cookie(&cookie);
        }
        let mut response = request.send().map_err(|e| self.out_of_time(transport(e)))?;
        if let Err(e) = check_status(&response) {
            // 状态不对时也留下回答的开头；读不出来就只报状态
            let head = response.read_to_vec(ANSWER_BYTES).unwrap_or_default();
            return Err(e.answered(&head));
        }
        let raw = read_limited(&mut response, API_BYTES).map_err(|e| self.out_of_time(e))?;
        let wire: Wire = serde_json::from_slice(&raw).map_err(|e| {
            ApiError::Shape(format!(
                "the body is not the expected JSON ({:?})",
                e.classify()
            ))
            .answered(&raw)
        })?;
        let code = wire
            .code
            .ok_or_else(|| ApiError::Shape("the response has no `code`".into()).answered(&raw))?;
        // 带 `v_voucher` 的响应是风控的验证要求，不管 code 是什么
        if code == -352 || !wire.data["v_voucher"].is_null() {
            return Err(ApiError::Code {
                code: -352,
                message: "risk control".into(),
            }
            .answered(&raw));
        }
        if code != 0 {
            return Err(ApiError::Code {
                code,
                message: wire.message.chars().take(MESSAGE_CHARS).collect(),
            }
            .answered(&raw));
        }
        Ok(Reply {
            data: wire.data,
            raw,
        })
    }

    fn open_media(&self, url: &Url, timeout: Duration) -> Result<http::Response, ApiError> {
        let response = self
            .endpoints
            .media_client()
            .get(url)
            .header("Referer", REFERER)
            .timeout(self.step_timeout(timeout)?)
            .send()
            .map_err(|e| self.out_of_time(transport(e)))?;
        check_status(&response)?;
        Ok(response)
    }

    /// 把一个文件流式存到 `dest`，最多 `limit` 字节。不带 cookie。每秒向宿主报一次进度。
    pub fn download(
        &self,
        url: &Url,
        dest: &Path,
        limit: u64,
        timeout: Duration,
        label: &str,
    ) -> Result<u64, ApiError> {
        let response = self.open_media(url, timeout)?;
        let total = response
            .header("content-length")
            .and_then(|v| v.parse::<u64>().ok());
        if total.is_some_and(|t| t > limit) {
            return Err(ApiError::TooLarge { limit });
        }
        let io = |source| ApiError::Io {
            path: dest.display().to_string(),
            source,
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let file = std::fs::File::create_new(dest).map_err(io)?;
        let written = copy_limited(response.into_reader(limit), file, limit, total, label);
        if written.is_err() {
            // 写了一半的文件不留在输出目录里：宿主会把没被引用的文件也原样收下。
            // 清理失败不改变这次调用的结果，调用失败时整个目录本来就会被丢掉。
            let _ = std::fs::remove_file(dest);
        }
        written.map_err(|e| match e {
            CopyError::Api(e) => self.out_of_time(e),
            CopyError::Io(source) => io(source),
        })
    }

    /// 展开短链：一跳一跳读 `Location`，只请求短链主机，一到视频页主机就停，不请求视频页。
    pub fn expand_short_link(&self, start: Url) -> Result<Url, ApiError> {
        let mut current = start;
        for _ in 0..MAX_SHORT_LINK_HOPS {
            let host = current
                .host_str()
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if is_video_host(&host) || !self.endpoints.is_short_link_host(&host) {
                return Ok(current);
            }
            let response = self
                .endpoints
                .short_link_client()
                .get(&current)
                .timeout(SHORT_LINK_TIMEOUT)
                .send()
                .map_err(transport)?;
            let status = response.status();
            if !(300..400).contains(&status) {
                check_status(&response)?;
                return Ok(current);
            }
            let location = response
                .header("location")
                .ok_or_else(|| ApiError::Shape("a redirect without `Location`".into()))?;
            let next = current
                .join(location)
                .map_err(|_| ApiError::Shape("`Location` is not a URL".into()))?;
            if !matches!(next.scheme(), "http" | "https") {
                return Ok(next);
            }
            current = next;
        }
        Err(ApiError::Shape("too many redirects".into()))
    }
}

enum CopyError {
    Api(ApiError),
    Io(std::io::Error),
}

/// 把响应体流式写进文件，`reader` 读出超过 `limit` 字节时报错，每秒报一次进度。
fn copy_limited(
    mut reader: impl Read,
    mut file: std::fs::File,
    limit: u64,
    total: Option<u64>,
    label: &str,
) -> Result<u64, CopyError> {
    let mut buffer = vec![0u8; 64 << 10];
    let mut written = 0u64;
    let mut last_report = Instant::now();
    loop {
        let n = reader.read(&mut buffer).map_err(|e| {
            CopyError::Api(if e.kind() == std::io::ErrorKind::FileTooLarge {
                ApiError::TooLarge { limit }
            } else {
                ApiError::Network(e.to_string())
            })
        })?;
        if n == 0 {
            break;
        }
        written += n as u64;
        file.write_all(&buffer[..n]).map_err(CopyError::Io)?;
        if last_report.elapsed() >= Duration::from_secs(1) {
            progress(written, total, label);
            last_report = Instant::now();
        }
    }
    file.flush().map_err(CopyError::Io)?;
    progress(written, total, label);
    Ok(written)
}

/// 开始一步之前告诉宿主：一步一步慢下来时，宿主照样收得到 progress，不按「没进度」杀掉进程。
pub(crate) fn step(label: &str) {
    let event = PluginEvent::Progress {
        done: None,
        total: None,
        unit: None,
        message: Some(label.into()),
    };
    eprintln!("{}", event.stderr_line());
}

fn progress(done: u64, total: Option<u64>, label: &str) {
    let event = PluginEvent::Progress {
        done: Some(done as f64),
        total: total.map(|t| t as f64),
        unit: Some("B".into()),
        message: Some(label.into()),
    };
    eprintln!("{}", event.stderr_line());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_hosts() {
        let e = Endpoints::production();
        let ok = |s: &str| e.media_url(s).map(|u| u.to_string());
        assert_eq!(
            ok("http://i2.hdslb.com/bfs/archive/a.jpg").unwrap(),
            "https://i2.hdslb.com/bfs/archive/a.jpg"
        );
        assert_eq!(
            ok("//aisubtitle.hdslb.com/bfs/subtitle/x.json?auth_key=1").unwrap(),
            "https://aisubtitle.hdslb.com/bfs/subtitle/x.json?auth_key=1"
        );
        assert!(ok("https://upos-sz-mirrorcos.bilivideo.com/upgcxcode/a.m4s").is_ok());
        assert!(ok("https://xy1x2x3x4xy.mcdn.bilivideo.cn/v1/resource/a.m4s").is_ok());
        assert!(ok("https://upos-sz.bilivideo.com:443/a.m4s").is_ok());
        // 非默认端口不在清单放行的范围里
        assert!(matches!(
            ok("https://xy1x2x3x4xy.mcdn.bilivideo.cn:4483/v1/resource/a.m4s"),
            Err(ApiError::ForbiddenHost(_))
        ));
        assert!(matches!(
            ok("https://evilhdslb.com/a"),
            Err(ApiError::ForbiddenHost(_))
        ));
        assert!(matches!(
            ok("https://hdslb.com.evil.test/a"),
            Err(ApiError::ForbiddenHost(_))
        ));
        assert!(matches!(
            ok("https://127.0.0.1/a"),
            Err(ApiError::ForbiddenHost(_))
        ));
        assert!(matches!(
            ok("ftp://i0.hdslb.com/a"),
            Err(ApiError::Shape(_))
        ));
        assert!(matches!(ok("nonsense"), Err(ApiError::Shape(_))));
    }

    #[test]
    fn failure_mapping() {
        let code = |c: i64| {
            CallFailure::from(ApiError::Code {
                code: c,
                message: "m".into(),
            })
        };
        assert_eq!(code(-412).code, ErrorCode::Blocked);
        assert_eq!(code(-352).code, ErrorCode::Blocked);
        let login = code(-101);
        assert_eq!(
            (login.code, login.site.as_deref()),
            (ErrorCode::LoginRequired, Some(SITE))
        );
        assert_eq!(code(-404).reason, Some(NotFoundReason::Deleted));
        assert_eq!(code(62012).reason, Some(NotFoundReason::Private));
        assert_eq!(code(-799).code, ErrorCode::RateLimited);
        assert_eq!(code(12345).code, ErrorCode::UpstreamChanged);
        let http = |status| {
            CallFailure::from(ApiError::Http {
                status,
                retry_after: Some(7),
            })
        };
        assert_eq!(http(412).code, ErrorCode::Blocked);
        assert_eq!(http(429).retry_after, Some(7));
        assert_eq!(http(502).code, ErrorCode::Network);
        assert_eq!(http(404).code, ErrorCode::NotFound);
    }
}
