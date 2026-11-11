//! 对着本机的假服务器跑：canonicalize 的短链展开、sources 和 list 的翻页、
//! fetch 带不带 cookie、错误码映射、输出目录里有什么。不碰真网络。

mod support;

use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use support::{Request, Response, Server, fixture};
use usuyo_fetch_bilibili::api::{ApiError, Bilibili, Endpoints, SITE};
use usuyo_fetch_bilibili::canonical::canonicalize;
use usuyo_fetch_bilibili::favorites;
use usuyo_fetch_bilibili::session::Session;
use usuyo_fetch_bilibili::video::{self, OUTPUTS};
use usuyo_plugin::protocol::{
    Access, CallFailure, Canonicalized, Completeness, ErrorCode, Identity, ListAction, MediaKind,
    NotFoundReason, OutputKind, Rfc3339,
};
use usuyo_plugin_kit::http::Policy;

const BV: &str = "BV17x411w7KC";
const SESSDATA: &str = "sessvalue1";

/// 只在测试里用的 BV 号：形状合法，各自触发一种服务器行为。
const COVER_404: &str = "BV1aaaaacov1";
const COVER_EVIL: &str = "BV1aaaaacov2";
const ERRORS: [(&str, ErrorCode); 10] = [
    ("BV1aaaaa4121", ErrorCode::Blocked),         // JSON code -412
    ("BV1aaaaa3521", ErrorCode::Blocked),         // JSON code -352
    ("BV1aaaaa1111", ErrorCode::LoginRequired),   // JSON code -101
    ("BV1aaaaa4441", ErrorCode::NotFound),        // JSON code -404
    ("BV1aaaaah412", ErrorCode::Blocked),         // HTTP 412
    ("BV1aaaaah429", ErrorCode::RateLimited),     // HTTP 429 + Retry-After
    ("BV1aaaaah5xx", ErrorCode::Network),         // HTTP 500
    ("BV1aaaaa9991", ErrorCode::UpstreamChanged), // JSON code 12345
    ("BV1aaaaavou1", ErrorCode::Blocked),         // code 0 但带 v_voucher
    ("BV1aaaaahttp", ErrorCode::UpstreamChanged), // 不是 JSON
];

fn credentials(sessdata: &str) -> Vec<u8> {
    format!(
        r#"{{"sites":[{{"site":"www.bilibili.com","source":"daily-browser"}}],"storageState":{{"cookies":[
        {{"name":"SESSDATA","value":"{sessdata}","domain":".bilibili.com","path":"/","expires":4102444800}},
        {{"name":"DedeUserID","value":"12345","domain":".bilibili.com","path":"/","expires":-1}},
        {{"name":"bili_jct","value":"csrfvalue","domain":".bilibili.com","path":"/","expires":-1}}
        ],"origins":[]}}}}"#
    )
    .into_bytes()
}

fn session() -> Session {
    Session::from_json(&credentials(SESSDATA), 1_000)
        .unwrap()
        .unwrap()
}

fn json_of(name: &str, base: &str) -> Response {
    Response::json(fixture(name, base))
}

fn view_response(req: &Request, base: &str) -> Response {
    let bvid = req
        .query
        .get("bvid")
        .map(String::as_str)
        .unwrap_or_default();
    let code = |c: i64| Response::json(format!(r#"{{"code":{c},"message":"m","ttl":1}}"#));
    match bvid {
        "BV1aaaaa4121" => code(-412),
        "BV1aaaaa3521" => code(-352),
        "BV1aaaaa1111" => code(-101),
        "BV1aaaaa4441" => code(-404),
        "BV1aaaaa9991" => code(12345),
        "BV1aaaaavou1" => {
            Response::json(r#"{"code":0,"message":"0","data":{"v_voucher":"voucher_x"}}"#)
        }
        "BV1aaaaah412" => Response::status(412),
        "BV1aaaaah429" => Response::status(429).header("Retry-After", "30"),
        "BV1aaaaah5xx" => Response::status(500),
        "BV1aaaaahttp" => Response::json("<html>risk</html>"),
        _ => {
            let mut body = fixture("view.json", base).replace(BV, bvid);
            if bvid == COVER_404 {
                body = body.replace("/img/cover.jpg", "/img/missing.jpg");
            }
            if bvid == COVER_EVIL {
                body = body.replace(
                    &format!("{base}/img/cover.jpg"),
                    "https://evil.example/c.jpg",
                );
            }
            Response::json(body)
        }
    }
}

/// 一个什么都有的 B站。
fn bilibili(req: &Request, base: &str) -> Response {
    match req.path.as_str() {
        "/x/web-interface/view" => view_response(req, base),
        "/x/player/playurl" => json_of("playurl.json", base),
        "/x/player/wbi/v2" => json_of("player_v2.json", base),
        "/img/cover.jpg" => Response::bytes(b"COVER"),
        "/cdn/video-32.m4s" => Response::bytes(b"VIDEO32"),
        "/cdn/audio-64.m4s" => Response::bytes(b"AUDIO64"),
        "/cdn/big" => Response::bytes(&[7u8; 4096]),
        "/cdn/nolen" => Response::bytes(&[7u8; 4096]).header("X-No-Length", "1"),
        "/sub/zh.json" => json_of("subtitle_zh.json", base),
        "/sub/en.json" => json_of("subtitle_en.json", base),
        "/x/v3/fav/folder/created/list-all" => json_of("folders.json", base),
        "/x/v3/fav/resource/list" => {
            match (req.query["media_id"].as_str(), req.query["pn"].as_str()) {
                ("100", "1") => json_of("fav_page1.json", base),
                ("100", "2") => json_of("fav_page2.json", base),
                _ => json_of("fav_empty.json", base),
            }
        }
        "/x/v2/history/toview/web" => json_of("toview.json", base),
        "/s1" => Response::redirect("/s2"),
        "/s2" => Response::redirect(
            "https://www.bilibili.com/video/BV17x411w7KC?share_source=copy_web&vd_source=abc",
        ),
        "/bangumi" => Response::redirect("https://www.bilibili.com/bangumi/play/ep1"),
        "/loop" => Response::redirect("/loop"),
        _ => Response::status(404),
    }
}

/// 测试用的地址策略：只有 127.0.0.1 算公网，不查 DNS。
/// `inner.hdslb.com` 在媒体主机名单里，但解析到内网地址。
fn local_policy() -> Policy {
    Policy {
        lookup: |host, port| match host {
            "inner.hdslb.com" => Ok(vec![std::net::SocketAddr::from(([10, 0, 0, 1], port))]),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no DNS in tests",
            )),
        },
        public: |ip| ip == IpAddr::from([127, 0, 0, 1]),
    }
}

fn api_for(server: &Server, session: Option<Session>) -> Bilibili {
    Bilibili::new(Endpoints::local(&server.base, local_policy()), session)
}

fn failure<T: std::fmt::Debug>(result: Result<T, CallFailure>) -> CallFailure {
    result.expect_err("expected a failure")
}

fn video_url(bvid: &str) -> String {
    format!("https://www.bilibili.com/video/{bvid}")
}

/// 输出目录里的全部文件，相对路径，排好序。
fn files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn content_id(identity: &Option<Identity>) -> String {
    match identity {
        Some(Identity::Platform {
            platform,
            content_id,
            ..
        }) => {
            assert_eq!(platform.as_str(), "bilibili");
            content_id.clone()
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn anonymous_fetch_writes_metadata_cover_and_both_streams() {
    let server = Server::start(bilibili);
    let api = api_for(&server, None);
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(&api, &video_url(BV), tmp.path(), None).unwrap();

    assert_eq!(content_id(&out.identity), BV);
    assert_eq!(out.title.as_deref(), Some("【MV】保加利亚妖王AZIS视频合辑"));
    assert!(out.text.as_deref().unwrap().contains("Azis"));
    let author = out.author.as_ref().unwrap();
    assert_eq!(
        (author.name.as_str(), author.platform_id.as_deref()),
        ("冰封.虾子", Some("122541"))
    );
    assert_eq!(
        out.published_at.as_ref().unwrap().as_str(),
        "2011-11-09T14:55:33Z"
    );
    assert_eq!(out.platform_tags, ["音乐综合"]);
    assert_eq!(
        out.images.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        ["images/cover.jpg"]
    );

    // 视频和音频是两个文件，都写出来，各占 media 里的一项
    let kinds: Vec<_> = out
        .media
        .iter()
        .map(|m| (m.kind, m.path.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            (MediaKind::Video, "media/video.m4s"),
            (MediaKind::Audio, "media/audio.m4s")
        ]
    );
    assert_eq!(out.media[0].duration_sec, Some(199.0));
    // 低画质：AVC 里 id 最小的（32，主地址不在允许的主机上，用备用地址），音频取 30216
    assert_eq!(
        std::fs::read(tmp.path().join("media/video.m4s")).unwrap(),
        b"VIDEO32"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("media/audio.m4s")).unwrap(),
        b"AUDIO64"
    );

    // 没有 cookie：字幕拿不到，如实记下
    assert_eq!(out.completeness, Completeness::Partial);
    assert_eq!(out.missing, [OutputKind::Subtitle]);
    assert!(out.missing_reason.as_deref().unwrap().contains("登录"));
    assert_eq!(out.evidence.access, Access::PublicWeb);
    assert_eq!(out.evidence.site, None);

    assert_eq!(
        files(tmp.path()),
        [
            "images/cover.jpg",
            "media/audio.m4s",
            "media/video.m4s",
            "meta.json",
            "view.json"
        ]
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tmp.path().join("meta.json")).unwrap()).unwrap();
    assert_eq!(
        (
            meta["cid"].as_u64(),
            meta["page"].as_u64(),
            meta["pageCount"].as_u64()
        ),
        (Some(279786), Some(1), Some(10))
    );
    assert!(
        meta["streams"]["note"]
            .as_str()
            .unwrap()
            .contains("没有合并")
    );
    let view: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tmp.path().join("view.json")).unwrap()).unwrap();
    assert_eq!(view["data"]["aid"], 170001);

    // 宿主会做的检查：输出照协议类型走一遍，引用的文件都在，缺的 = 想要的 − 拿到的
    let again: usuyo_plugin::protocol::FetchOutput =
        serde_json::from_value(serde_json::to_value(&out).unwrap()).unwrap();
    assert_eq!(again, out);
    for path in out.referenced_files() {
        assert!(tmp.path().join(path.as_str()).is_file(), "{path}");
    }
    let wanted: std::collections::BTreeSet<_> = OUTPUTS.into();
    assert_eq!(
        wanted
            .difference(&out.obtained())
            .copied()
            .collect::<Vec<_>>(),
        out.missing
    );

    let requests = server.requests();
    assert!(requests.iter().all(|r| r.cookie().is_none()));
    assert!(!server.paths().contains(&"/x/player/wbi/v2".to_owned()));
    let playurl = requests
        .iter()
        .find(|r| r.path == "/x/player/playurl")
        .unwrap();
    assert_eq!(playurl.query["cid"], "279786");
    assert_eq!(playurl.query["bvid"], BV);
    assert_eq!(
        (
            playurl.query["fnval"].as_str(),
            playurl.query["avid"].as_str()
        ),
        ("16", "170001")
    );
}

#[test]
fn fetch_with_cookies_adds_subtitles_and_sends_cookies_only_to_the_api() {
    let server = Server::start(bilibili);
    let api = api_for(&server, Some(session()));
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(&api, &video_url(BV), tmp.path(), None).unwrap();

    assert_eq!(
        out.completeness,
        Completeness::Full,
        "{:?} {:?}",
        out.missing,
        out.missing_reason
    );
    assert!(out.missing.is_empty() && out.missing_reason.is_none());
    assert_eq!(out.evidence.access, Access::SessionApi);
    assert_eq!(out.evidence.site.as_deref(), Some(SITE));
    let subs: Vec<_> = out
        .media
        .iter()
        .filter(|m| m.kind == MediaKind::Subtitle)
        .map(|m| (m.path.as_str(), m.lang.as_deref().unwrap()))
        .collect();
    assert_eq!(
        subs,
        [
            ("media/subtitle.zh-CN.vtt", "zh-CN"),
            ("media/subtitle.ai-en.vtt", "ai-en")
        ]
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("media/subtitle.zh-CN.vtt")).unwrap(),
        "WEBVTT\n\n00:00:00.500 --> 00:00:02.250\n第一句\n\n01:01:01.000 --> 01:01:02.500\n含有 -> 箭头\n和空行\n\n"
    );
    assert_eq!(
        files(tmp.path()),
        [
            "images/cover.jpg",
            "media/audio.m4s",
            "media/subtitle.ai-en.vtt",
            "media/subtitle.zh-CN.vtt",
            "media/video.m4s",
            "meta.json",
            "view.json"
        ]
    );

    let expected = format!("SESSDATA={SESSDATA}; DedeUserID=12345");
    for request in server.requests() {
        let on_api = request.path.starts_with("/x/");
        match request.cookie() {
            Some(cookie) => {
                assert!(on_api, "cookie sent to {}", request.path);
                // bili_jct 是写操作用的，读不带
                assert_eq!(cookie, expected, "{}", request.path);
            }
            None => assert!(!on_api, "no cookie on {}", request.path),
        }
    }
}

#[test]
fn page_parameter_selects_the_part() {
    let server = Server::start(bilibili);
    let api = api_for(&server, None);
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(
        &api,
        &format!("{}?p=2&spm_id_from=x", video_url(BV)),
        tmp.path(),
        Some(&[OutputKind::Text, OutputKind::Meta]),
    )
    .unwrap();
    assert_eq!(content_id(&out.identity), format!("{BV}:p2"));
    assert_eq!(
        out.title.as_deref(),
        Some("【MV】保加利亚妖王AZIS视频合辑 P2 Imash li surce")
    );
    assert_eq!(out.completeness, Completeness::Full);

    let tmp = tempfile::tempdir().unwrap();
    video::fetch(
        &api,
        &format!("{}?p=3", video_url(BV)),
        tmp.path(),
        Some(&[OutputKind::Audio]),
    )
    .unwrap();
    let playurl = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/x/player/playurl")
        .unwrap();
    assert_eq!(playurl.query["cid"], "279787");

    let tmp = tempfile::tempdir().unwrap();
    let gone = failure(video::fetch(
        &api,
        &format!("{}?p=11", video_url(BV)),
        tmp.path(),
        None,
    ));
    assert_eq!(gone.code, ErrorCode::NotFound);
}

#[test]
fn want_limits_what_is_fetched() {
    let server = Server::start(bilibili);
    let api = api_for(&server, None);

    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(
        &api,
        &video_url(BV),
        tmp.path(),
        Some(&[OutputKind::Text, OutputKind::Meta]),
    )
    .unwrap();
    assert_eq!(out.completeness, Completeness::Full);
    assert_eq!(files(tmp.path()), ["meta.json", "view.json"]);
    assert!(out.images.is_empty() && out.media.is_empty());
    assert_eq!(server.paths(), ["/x/web-interface/view"]);

    // 要视频就一起写音频
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(&api, &video_url(BV), tmp.path(), Some(&[OutputKind::Video])).unwrap();
    assert_eq!(out.media.len(), 2);
    assert_eq!(out.completeness, Completeness::Full);

    // 只要音频就不下视频流
    let before = server.paths().len();
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(&api, &video_url(BV), tmp.path(), Some(&[OutputKind::Audio])).unwrap();
    assert_eq!(
        out.media.iter().map(|m| m.kind).collect::<Vec<_>>(),
        [MediaKind::Audio]
    );
    let later: Vec<_> = server.paths().split_off(before);
    assert!(!later.iter().any(|p| p.contains("video")), "{later:?}");

    let tmp = tempfile::tempdir().unwrap();
    let bad = failure(video::fetch(
        &api,
        &video_url(BV),
        tmp.path(),
        Some(&[OutputKind::Html]),
    ));
    assert_eq!(bad.code, ErrorCode::InvalidArgs);
    assert!(OUTPUTS.contains(&OutputKind::Subtitle));
}

#[test]
fn optional_parts_that_fail_become_missing_not_errors() {
    let server = Server::start(bilibili);
    let api = api_for(&server, None);
    for (bvid, needle) in [(COVER_404, "404"), (COVER_EVIL, "evil.example")] {
        let tmp = tempfile::tempdir().unwrap();
        let out = video::fetch(
            &api,
            &video_url(bvid),
            tmp.path(),
            Some(&[OutputKind::Text, OutputKind::Images]),
        )
        .unwrap();
        assert_eq!(out.completeness, Completeness::Partial);
        assert_eq!(out.missing, [OutputKind::Images]);
        let reason = out.missing_reason.unwrap();
        assert!(
            reason.contains("封面") && reason.contains(needle),
            "{reason}"
        );
        assert!(out.images.is_empty());
    }
    assert!(!server.paths().iter().any(|p| p.contains("evil")));

    // 老视频：playurl 没有 DASH
    let old = Server::start(|req, base| match req.path.as_str() {
        "/x/player/playurl" => Response::json(
            r#"{"code":0,"message":"0","data":{"format":"flv","durl":[{"url":"x"}]}}"#,
        ),
        _ => bilibili(req, base),
    });
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(
        &api_for(&old, None),
        &video_url(BV),
        tmp.path(),
        Some(&[OutputKind::Video]),
    )
    .unwrap();
    assert_eq!(out.completeness, Completeness::Partial);
    assert_eq!(out.missing, [OutputKind::Video, OutputKind::Audio]);
    assert!(out.missing_reason.unwrap().contains("DASH"));
}

#[test]
fn each_subtitle_is_handled_on_its_own() {
    // 英文字幕 404：只跳过它，中文照写
    let server = Server::start(|req, base| match req.path.as_str() {
        "/sub/en.json" => Response::status(404),
        _ => bilibili(req, base),
    });
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(
        &api_for(&server, Some(session())),
        &video_url(BV),
        tmp.path(),
        Some(&[OutputKind::Subtitle]),
    )
    .unwrap();
    assert_eq!(out.completeness, Completeness::Full);
    assert_eq!(out.media.len(), 1);
    assert_eq!(
        files(tmp.path()),
        ["media/subtitle.zh-CN.vtt", "meta.json", "view.json"]
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tmp.path().join("meta.json")).unwrap()).unwrap();
    assert!(
        meta["subtitleSkipped"][0]
            .as_str()
            .unwrap()
            .starts_with("ai-en")
    );

    // 两条都不可用：没有字幕文件，原因记进 missingReason
    let server = Server::start(|req, base| match req.path.as_str() {
        "/sub/en.json" | "/sub/zh.json" => Response::json("{}"),
        _ => bilibili(req, base),
    });
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(
        &api_for(&server, Some(session())),
        &video_url(BV),
        tmp.path(),
        Some(&[OutputKind::Subtitle]),
    )
    .unwrap();
    assert_eq!(out.missing, [OutputKind::Subtitle]);
    assert!(out.missing_reason.unwrap().contains("zh-CN"));
    assert_eq!(files(tmp.path()), ["meta.json", "view.json"]);

    // 第二条遇到风控：整次报 blocked
    let server = Server::start(|req, base| match req.path.as_str() {
        "/sub/en.json" => Response::status(412),
        _ => bilibili(req, base),
    });
    let tmp = tempfile::tempdir().unwrap();
    let f = failure(video::fetch(
        &api_for(&server, Some(session())),
        &video_url(BV),
        tmp.path(),
        Some(&[OutputKind::Subtitle]),
    ));
    assert_eq!(f.code, ErrorCode::Blocked);
}

#[test]
fn risk_control_in_an_optional_step_stops_the_call() {
    let server = Server::start(|req, base| match req.path.as_str() {
        "/x/player/wbi/v2" => Response::json(r#"{"code":-412,"message":"blocked"}"#),
        _ => bilibili(req, base),
    });
    let api = api_for(&server, Some(session()));
    let tmp = tempfile::tempdir().unwrap();
    let f = failure(video::fetch(&api, &video_url(BV), tmp.path(), None));
    assert_eq!(
        (f.code, f.site.as_deref()),
        (ErrorCode::Blocked, Some(SITE))
    );
}

#[test]
fn error_codes_are_mapped() {
    let server = Server::start(
        |req, base| match req.query.get("bvid").map(String::as_str) {
            Some(bvid) if bvid.contains("big") => Response::bytes(&vec![b' '; 9 << 20]),
            _ => bilibili(req, base),
        },
    );
    let api = api_for(&server, None);
    for (bvid, code) in ERRORS {
        let tmp = tempfile::tempdir().unwrap();
        let f = failure(video::fetch(&api, &video_url(bvid), tmp.path(), None));
        assert_eq!(f.code, code, "{bvid}: {f}");
        assert!(
            files(tmp.path()).is_empty(),
            "{bvid}: nothing is written when the view call fails"
        );
    }
    let tmp = tempfile::tempdir().unwrap();
    let f = failure(video::fetch(
        &api,
        &video_url("BV1aaaaa4441"),
        tmp.path(),
        None,
    ));
    assert_eq!(f.reason, Some(NotFoundReason::Deleted));
    let f = failure(video::fetch(
        &api,
        &video_url("BV1aaaaah429"),
        tmp.path(),
        None,
    ));
    assert_eq!(f.retry_after, Some(30));
    let f = failure(video::fetch(
        &api,
        &video_url("BV1aaaaa1111"),
        tmp.path(),
        None,
    ));
    assert_eq!(f.site.as_deref(), Some(SITE));
    // 响应超过大小上限
    let f = failure(video::fetch(
        &api,
        &video_url("BV1aaaaabig1"),
        tmp.path(),
        None,
    ));
    assert_eq!(f.code, ErrorCode::UpstreamChanged);
    let f = failure(video::fetch(
        &api,
        "https://example.com/video/BV17x411w7KC",
        tmp.path(),
        None,
    ));
    assert_eq!(f.code, ErrorCode::UnsupportedInput);
}

#[test]
fn downloads_stop_at_the_size_cap() {
    let server = Server::start(bilibili);
    let api = api_for(&server, None);
    let url = url::Url::parse(&format!("{}/cdn/big", server.base)).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let err = api
        .download(
            &url,
            &tmp.path().join("a.bin"),
            1024,
            Duration::from_secs(5),
            "t",
        )
        .unwrap_err();
    assert!(matches!(err, ApiError::TooLarge { limit: 1024 }), "{err:?}");
    assert!(!tmp.path().join("a.bin").exists());
    // 没有 Content-Length 的响应写到一半才发现太大：写了一半的文件要删掉
    let url = url::Url::parse(&format!("{}/cdn/nolen", server.base)).unwrap();
    let err = api
        .download(
            &url,
            &tmp.path().join("c.bin"),
            1024,
            Duration::from_secs(5),
            "t",
        )
        .unwrap_err();
    assert!(matches!(err, ApiError::TooLarge { limit: 1024 }), "{err:?}");
    assert!(!tmp.path().join("c.bin").exists());
    let url = url::Url::parse(&format!("{}/cdn/big", server.base)).unwrap();
    let written = api
        .download(
            &url,
            &tmp.path().join("b.bin"),
            4096,
            Duration::from_secs(5),
            "t",
        )
        .unwrap();
    assert_eq!(written, 4096);
}

#[test]
fn canonicalize_expands_short_links_without_cookies() {
    let server = Server::start(bilibili);
    let api = api_for(&server, Some(session()));

    let found = canonicalize(&api, &format!("{}/s1", server.base)).unwrap();
    let Canonicalized::Match {
        identity,
        resolved_url,
    } = found
    else {
        panic!("{found:?}")
    };
    assert_eq!(content_id(&Some(identity)), BV);
    assert_eq!(
        resolved_url.as_deref(),
        Some("https://www.bilibili.com/video/BV17x411w7KC?share_source=copy_web&vd_source=abc")
    );
    assert_eq!(server.paths(), ["/s1", "/s2"]);
    assert!(server.requests().iter().all(|r| r.cookie().is_none()));

    // 展开成番剧页：不是我们的视频
    assert_eq!(
        canonicalize(&api, &format!("{}/bangumi", server.base)).unwrap(),
        Canonicalized::NoMatch
    );
    // 死链
    assert_eq!(
        failure(canonicalize(&api, &format!("{}/dead", server.base))).code,
        ErrorCode::NotFound
    );
    // 绕圈
    assert_eq!(
        failure(canonicalize(&api, &format!("{}/loop", server.base))).code,
        ErrorCode::UpstreamChanged
    );

    // 链接里直接带 id 的不发请求
    let before = server.paths().len();
    let direct = canonicalize(&api, &format!("{}/av170001?p=2", server.base)).unwrap();
    let Canonicalized::Match {
        identity,
        resolved_url: None,
    } = direct
    else {
        panic!("{direct:?}")
    };
    assert_eq!(content_id(&Some(identity)), format!("{BV}:p2"));
    assert_eq!(server.paths().len(), before);

    // 离线
    let offline = Bilibili::new(Endpoints::local("http://127.0.0.1:1", local_policy()), None);
    assert_eq!(
        failure(canonicalize(&offline, "http://127.0.0.1:1/abc")).code,
        ErrorCode::Network
    );
    // 不是 B站的链接是 match: false，不是错误
    assert_eq!(
        canonicalize(&offline, "https://example.com/x").unwrap(),
        Canonicalized::NoMatch
    );
}

#[test]
fn sources_lists_my_folders_and_watch_later() {
    let server = Server::start(bilibili);
    let api = api_for(&server, Some(session()));
    let out = favorites::sources(&api).unwrap();
    let sources: Vec<_> = out
        .sources
        .iter()
        .map(|s| (s.source.as_str(), s.title.as_str(), s.count))
        .collect();
    assert_eq!(
        sources,
        [
            ("bilibili:fav:100", "默认收藏夹", Some(3)),
            ("bilibili:fav:200", "学习", Some(1)),
            ("bilibili:toview", "稍后再看", None)
        ]
    );
    // mid 来自 DedeUserID
    assert_eq!(server.requests()[0].query["up_mid"], "12345");

    let anonymous = Server::start(bilibili);
    let f = failure(favorites::sources(&api_for(&anonymous, None)));
    assert_eq!(
        (f.code, f.site.as_deref()),
        (ErrorCode::LoginRequired, Some(SITE))
    );
    assert!(anonymous.requests().is_empty());
}

#[test]
fn list_pages_through_a_folder() {
    let server = Server::start(bilibili);
    let api = api_for(&server, Some(session()));

    let page = favorites::list(&api, "bilibili:fav:100", None, None).unwrap();
    // 失效的和不是视频的跳过
    let titles: Vec<_> = page
        .items
        .iter()
        .map(|i| i.title.as_deref().unwrap())
        .collect();
    assert_eq!(titles, ["第一条", "第二条"]);
    assert_eq!(page.next_cursor.as_deref(), Some("2"));
    let first = &page.items[0];
    assert_eq!(
        first.url.as_deref(),
        Some("https://www.bilibili.com/video/BV17x411w7KC")
    );
    assert_eq!(content_id(&first.identity), BV);
    assert_eq!(first.action, Some(ListAction::Favorite));
    assert_eq!(
        first.acted_at.as_ref().unwrap().as_str(),
        "2026-10-26T07:38:20Z"
    );
    assert_eq!(
        first.author.as_ref().unwrap().platform_id.as_deref(),
        Some("122541")
    );
    assert_eq!(page.evidence.access, Access::SessionApi);
    let request = &server.requests()[0];
    assert_eq!(
        (
            request.query["media_id"].as_str(),
            request.query["pn"].as_str(),
            request.query["ps"].as_str(),
            request.query["order"].as_str()
        ),
        ("100", "1", "20", "mtime")
    );

    let page = favorites::list(&api, "bilibili:fav:100", Some("2"), None).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next_cursor, None);
    assert_eq!(server.requests().last().unwrap().query["pn"], "2");

    // --since：收藏时间早于它的不要，也不再翻页
    let since = Rfc3339::try_from("2026-10-26T07:34:00Z".to_owned()).unwrap();
    let page = favorites::list(&api, "bilibili:fav:100", None, Some(&since)).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next_cursor, None);

    // 空收藏夹：medias 是 null
    let page = favorites::list(&api, "bilibili:fav:200", None, None).unwrap();
    assert!(page.items.is_empty() && page.next_cursor.is_none());

    for (source, cursor, code) in [
        ("bilibili:fav:100", Some("abc"), ErrorCode::InvalidArgs),
        ("bilibili:fav:100", Some("0"), ErrorCode::InvalidArgs),
        ("bilibili:toview", Some("2"), ErrorCode::InvalidArgs),
        ("bilibili:fav:12x", None, ErrorCode::UnsupportedInput),
        ("bilibili:fav:", None, ErrorCode::UnsupportedInput),
        ("file:x.zip", None, ErrorCode::UnsupportedInput),
    ] {
        assert_eq!(
            failure(favorites::list(&api, source, cursor, None)).code,
            code,
            "{source} {cursor:?}"
        );
    }

    let anonymous = Server::start(bilibili);
    let f = failure(favorites::list(
        &api_for(&anonymous, None),
        "bilibili:fav:100",
        None,
        None,
    ));
    assert_eq!(f.code, ErrorCode::LoginRequired);
    assert!(anonymous.requests().is_empty());
}

#[test]
fn watch_later_is_one_page_newest_first() {
    let server = Server::start(bilibili);
    let api = api_for(&server, Some(session()));
    let page = favorites::list(&api, "bilibili:toview", None, None).unwrap();
    let titles: Vec<_> = page
        .items
        .iter()
        .map(|i| i.title.as_deref().unwrap())
        .collect();
    // 接口给的顺序不作数，按加入时间从新到旧；没有合法 BV 号的跳过
    assert_eq!(titles, ["最新", "较早"]);
    assert_eq!(page.next_cursor, None);
    assert_eq!(
        page.items[0].acted_at.as_ref().unwrap().as_str(),
        "2026-10-26T07:41:40Z"
    );

    let since = Rfc3339::try_from("2026-10-26T07:35:00Z".to_owned()).unwrap();
    let page = favorites::list(&api, "bilibili:toview", None, Some(&since)).unwrap();
    assert_eq!(page.items.len(), 1);
}

#[test]
fn list_errors_from_the_site_are_mapped() {
    let server = Server::start(|_, _| {
        Response::json(r#"{"code":-101,"message":"账号未登录","data":{"isLogin":false}}"#)
    });
    let api = api_for(&server, Some(session()));
    let f = failure(favorites::list(&api, "bilibili:fav:100", None, None));
    assert_eq!(f.code, ErrorCode::LoginRequired);
    let f = failure(favorites::sources(&api));
    assert_eq!(f.code, ErrorCode::LoginRequired);
}

#[test]
fn redirects_off_the_allowed_hosts_are_not_followed() {
    // 允许的主机跳去名单外的主机、跳去回环地址：媒体下载和短链展开都不跟过去
    let elsewhere = Server::start(|_, _| Response::bytes(b"ELSEWHERE"));
    let port = elsewhere.base.rsplit(':').next().unwrap().to_owned();
    let targets = [
        format!("http://localhost:{port}/c.jpg"),
        format!("http://127.0.0.2:{port}/c.jpg"),
    ];
    let server = Server::start(move |req, _| match req.path.as_str() {
        "/cdn/to-0" | "/s-0" => Response::redirect(&targets[0]),
        "/cdn/to-1" | "/s-1" => Response::redirect(&targets[1]),
        _ => Response::status(404),
    });
    let api = api_for(&server, Some(session()));
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..2 {
        let url = url::Url::parse(&format!("{}/cdn/to-{i}", server.base)).unwrap();
        let dest = tmp.path().join(format!("{i}.jpg"));
        let err = api
            .download(&url, &dest, 1024, Duration::from_secs(5), "t")
            .unwrap_err();
        assert!(matches!(err, ApiError::Http { status: 302, .. }), "{err:?}");
        assert!(!dest.exists());
        assert_eq!(
            canonicalize(&api, &format!("{}/s-{i}", server.base)).unwrap(),
            Canonicalized::NoMatch
        );
    }
    assert!(elsewhere.requests().is_empty(), "{:?}", elsewhere.paths());
    // 直接给名单外的地址或回环地址，连之前就拒
    for target in [
        format!("http://localhost:{port}/c.jpg"),
        format!("http://127.0.0.2:{port}/c.jpg"),
    ] {
        let url = url::Url::parse(&target).unwrap();
        let err = api
            .download(
                &url,
                &tmp.path().join("x.jpg"),
                1024,
                Duration::from_secs(5),
                "t",
            )
            .unwrap_err();
        assert!(matches!(err, ApiError::Refused(_)), "{err:?}");
    }
    assert!(elsewhere.requests().is_empty(), "{:?}", elsewhere.paths());
}

#[test]
fn a_media_address_that_resolves_inside_falls_back_to_the_backup() {
    // 主地址在允许的媒体主机上，但解析到内网：连之前就拒，改用备用地址
    let server = Server::start(|req, base| match req.path.as_str() {
        "/x/player/playurl" => Response::json(fixture("playurl.json", base).replace(
            "https://evil.example/video-32.m4s",
            "https://inner.hdslb.com/video-32.m4s",
        )),
        _ => bilibili(req, base),
    });
    let api = api_for(&server, None);
    let tmp = tempfile::tempdir().unwrap();
    let out = video::fetch(&api, &video_url(BV), tmp.path(), Some(&[OutputKind::Video])).unwrap();
    assert_eq!(out.missing, Vec::<OutputKind>::new());
    assert_eq!(
        std::fs::read(tmp.path().join("media/video.m4s")).unwrap(),
        b"VIDEO32"
    );
    assert!(server.paths().contains(&"/cdn/video-32.m4s".to_owned()));
}
