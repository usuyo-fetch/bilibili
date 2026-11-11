//! 宿主交来的登录信息（协议 §6）。cookie 的值只放在这里，不进日志、不进错误信息。

use std::fmt;
use std::io::Read;

use serde::Deserialize;

use crate::api::ApiError;

/// 凭据文件最大 1 MiB。
const CREDENTIALS_BYTES: u64 = 1 << 20;
/// 读接口用得到的 cookie 名。`bili_jct` 是写操作的 CSRF 令牌，读不需要，不带。
const SENT_COOKIES: [&str; 5] = [
    "SESSDATA",
    "DedeUserID",
    "DedeUserID__ckMd5",
    "buvid3",
    "buvid4",
];
const SESSION_COOKIE: &str = "SESSDATA";
const USER_ID_COOKIE: &str = "DedeUserID";

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "storageState", default)]
    storage_state: StorageState,
}

#[derive(Deserialize, Default)]
struct StorageState {
    #[serde(default)]
    cookies: Vec<Cookie>,
}

#[derive(Deserialize)]
struct Cookie {
    name: String,
    value: String,
    #[serde(default)]
    domain: String,
    /// Playwright 的写法：秒，会话 cookie 是 -1。
    #[serde(default = "session_cookie")]
    expires: f64,
}

fn session_cookie() -> f64 {
    -1.0
}

/// 一次调用里用到的登录态。
pub struct Session {
    cookie_header: String,
    user_id: Option<String>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// 读 `USUYO_CREDENTIALS` 指向的文件；没有这个变量就是没登录。
    pub fn from_env() -> Result<Option<Session>, ApiError> {
        let Some(path) = std::env::var_os("USUYO_CREDENTIALS") else {
            return Ok(None);
        };
        let file = std::fs::File::open(&path)
            .map_err(|e| ApiError::Credentials(format!("cannot open the credentials file: {e}")))?;
        let mut bytes = Vec::new();
        file.take(CREDENTIALS_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| ApiError::Credentials(format!("cannot read the credentials file: {e}")))?;
        if bytes.len() as u64 > CREDENTIALS_BYTES {
            return Err(ApiError::Credentials(
                "the credentials file is too large".into(),
            ));
        }
        Self::from_json(&bytes, jiff::Timestamp::now().as_second())
    }

    /// 取出 `.bilibili.com` 下没过期的 cookie；没有 `SESSDATA` 就当没登录。
    pub fn from_json(bytes: &[u8], now_secs: i64) -> Result<Option<Session>, ApiError> {
        // serde 的错误文字里会带出读到的值，所以只报位置和类别
        let credentials: Credentials = serde_json::from_slice(bytes).map_err(|e| {
            ApiError::Credentials(format!(
                "the credentials file is not valid ({:?} at line {} column {})",
                e.classify(),
                e.line(),
                e.column()
            ))
        })?;
        let mut pairs = Vec::new();
        let mut user_id = None;
        let mut has_session = false;
        for cookie in credentials.storage_state.cookies {
            let domain = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
            let on_site = domain == "bilibili.com" || domain.ends_with(".bilibili.com");
            let expired = cookie.expires > 0.0 && cookie.expires < now_secs as f64;
            if !on_site || expired || !SENT_COOKIES.contains(&cookie.name.as_str()) {
                continue;
            }
            if !header_safe(&cookie.value) || cookie.value.is_empty() {
                continue;
            }
            match cookie.name.as_str() {
                SESSION_COOKIE => has_session = true,
                USER_ID_COOKIE => {
                    user_id = (cookie.value.bytes().all(|b| b.is_ascii_digit())
                        && cookie.value.len() <= 20)
                        .then(|| cookie.value.clone());
                }
                _ => {}
            }
            pairs.push(format!("{}={}", cookie.name, cookie.value));
        }
        Ok(has_session.then(|| Session {
            cookie_header: pairs.join("; "),
            user_id,
        }))
    }

    /// 只给发往 B站 API 的请求用。
    pub fn cookie_header(&self) -> &str {
        &self.cookie_header
    }

    /// `DedeUserID`，我自己的 mid。
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }
}

/// 能放进 `Cookie` 请求头的值：可见 ASCII，没有分号、逗号、引号、反斜杠。
fn header_safe(value: &str) -> bool {
    value
        .bytes()
        .all(|b| (0x21..0x7f).contains(&b) && !matches!(b, b';' | b',' | b'"' | b'\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str, domain: &str, expires: i64) -> String {
        format!(
            r#"{{"name":"{name}","value":"{value}","domain":"{domain}","path":"/","expires":{expires}}}"#
        )
    }

    fn state(cookies: &[String]) -> Vec<u8> {
        format!(
            r#"{{"sites":[],"storageState":{{"cookies":[{}],"origins":[]}}}}"#,
            cookies.join(",")
        )
        .into_bytes()
    }

    #[test]
    fn keeps_unexpired_read_cookies_for_the_site_only() {
        let bytes = state(&[
            cookie("SESSDATA", "s3cret%2Cvalue", ".bilibili.com", 4_000_000_000),
            cookie("DedeUserID", "12345", ".bilibili.com", -1),
            cookie("bili_jct", "csrf", ".bilibili.com", -1),
            cookie("SESSDATA", "other", ".example.com", -1),
            cookie("buvid3", "old", ".bilibili.com", 10),
        ]);
        let session = Session::from_json(&bytes, 1_000).unwrap().unwrap();
        assert_eq!(
            session.cookie_header(),
            "SESSDATA=s3cret%2Cvalue; DedeUserID=12345"
        );
        assert_eq!(session.user_id(), Some("12345"));
        assert!(!format!("{session:?}").contains("s3cret"));
    }

    #[test]
    fn no_session_cookie_means_not_logged_in() {
        let bytes = state(&[cookie("DedeUserID", "12345", ".bilibili.com", -1)]);
        assert!(Session::from_json(&bytes, 0).unwrap().is_none());
        let expired = state(&[cookie("SESSDATA", "x", ".bilibili.com", 5)]);
        assert!(Session::from_json(&expired, 100).unwrap().is_none());
    }

    #[test]
    fn rejects_values_that_could_break_the_header() {
        let bytes = state(&[
            cookie("SESSDATA", "ok", ".bilibili.com", -1),
            cookie("DedeUserID", "12;345", ".bilibili.com", -1),
        ]);
        let session = Session::from_json(&bytes, 0).unwrap().unwrap();
        assert_eq!(session.cookie_header(), "SESSDATA=ok");
        assert_eq!(session.user_id(), None);
    }

    #[test]
    fn garbage_is_an_error_without_content() {
        let err =
            Session::from_json(b"{\"storageState\": 5, \"x\":\"SESSDATA-leak\"}", 0).unwrap_err();
        assert!(!err.to_string().contains("SESSDATA-leak"));
    }
}
