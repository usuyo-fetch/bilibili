//! `fetch`：一个视频的元数据、封面，和按需要的低画质音视频流。不带账号：
//! 这里的请求都不带 cookie，也不读宿主交来的凭据。
//!
//! 先取视频页，读页面里的视频信息和流地址；页面里读不到的那一半再调不签名的接口
//! （`view`、`playurl`）补上。`meta.json` 的 `stage` 记下两半各是从哪里取到的。
//!
//! 输出目录里的文件名都是固定的，不用网络上来的名字：
//! `view.json`（视频信息的原始 JSON：页面那一路是 `__INITIAL_STATE__.videoData`，
//! 接口那一路是 `view` 接口的整个响应）、`meta.json`、`images/cover.<ext>`、
//! `media/video.m4s`、`media/audio.m4s`。

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use usuyo_plugin_kit::protocol::{
    Access, Author, CallFailure, Completeness, ErrorCode, Evidence, FetchOutput, ItemKind, Media,
    MediaGroup, MediaKind, OutputKind, RelPath, Rfc3339, TextFormat,
};

use crate::api::{ApiError, Bilibili, step};
use crate::canonical::{identity, locate};
use crate::ids::VideoRef;

/// 本插件能给的输出，和清单里的 `outputs` 一致。
pub const OUTPUTS: [OutputKind; 5] = [
    OutputKind::Text,
    OutputKind::Images,
    OutputKind::Video,
    OutputKind::Audio,
    OutputKind::Meta,
];

const COVER_BYTES: u64 = 20 << 20;
const COVER_TIMEOUT: Duration = Duration::from_secs(30);
const VIDEO_BYTES: u64 = 1 << 30;
const VIDEO_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const AUDIO_BYTES: u64 = 256 << 20;
const AUDIO_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// 每一路最多试几个备用地址：接口给再多也不变成一串请求。
const MAX_BACKUPS: usize = 3;
/// playurl 里视频的编码 id：7 是 AVC，哪里都能解。
const CODEC_AVC: u64 = 7;

#[derive(Deserialize)]
struct View {
    bvid: String,
    aid: u64,
    title: String,
    #[serde(default)]
    pic: String,
    #[serde(default)]
    pubdate: i64,
    #[serde(default)]
    desc: String,
    #[serde(default)]
    tname: String,
    owner: Owner,
    pages: Vec<Page>,
}

#[derive(Deserialize)]
struct Owner {
    mid: u64,
    name: String,
}

#[derive(Deserialize)]
struct Page {
    cid: u64,
    page: u32,
    #[serde(default)]
    part: String,
    #[serde(default)]
    duration: f64,
}

#[derive(Deserialize)]
struct Play {
    dash: Option<Dash>,
}

#[derive(Deserialize)]
struct Dash {
    #[serde(default)]
    video: Vec<DashStream>,
    #[serde(default)]
    audio: Vec<DashStream>,
}

#[derive(Deserialize, Clone)]
struct DashStream {
    id: u64,
    #[serde(default)]
    codecid: u64,
    #[serde(default)]
    bandwidth: u64,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(rename = "baseUrl", default)]
    base_url: String,
    #[serde(rename = "backupUrl", default)]
    backup_url: Option<Vec<String>>,
}

pub fn fetch(
    api: &Bilibili,
    input: &str,
    out: &Path,
    want: Option<&[OutputKind]>,
) -> Result<FetchOutput, CallFailure> {
    let mut wanted: BTreeSet<OutputKind> =
        want.map_or_else(|| OUTPUTS.into(), |w| w.iter().copied().collect());
    if let Some(other) = wanted.iter().find(|w| !OUTPUTS.contains(w)) {
        return Err(CallFailure::new(
            ErrorCode::InvalidArgs,
            format!("cannot give `{other}`"),
        ));
    }
    // 视频流和音频流是一对，要视频就一起取音频
    if wanted.contains(&OutputKind::Video) {
        wanted.insert(OutputKind::Audio);
    }
    let found = locate(api, input)?.ok_or_else(|| {
        CallFailure::new(ErrorCode::UnsupportedInput, "not a bilibili video link")
    })?;
    let video = found.video;
    let identity = identity(&video)?;

    let fetched_at = Rfc3339::from_timestamp(jiff::Timestamp::now());
    step("视频页");
    let from_page = video_page(api, &video)?;
    let (view, view_raw, view_from) = match from_page.view {
        Some((view, raw)) => (view, raw, PAGE),
        None => {
            step("视频信息");
            let reply = api.get("x/web-interface/view", &[("bvid", video.bvid.as_str())])?;
            (reply.data()?, reply.raw, API)
        }
    };
    if view.bvid != video.bvid {
        return Err(CallFailure::new(
            ErrorCode::UpstreamChanged,
            "`view` answered for a different video",
        ));
    }
    let page = view
        .pages
        .iter()
        .find(|p| p.page == video.page)
        .ok_or_else(|| {
            CallFailure::new(
                ErrorCode::NotFound,
                format!("the video has no part {}", video.page),
            )
        })?;
    write(out, "view.json", &view_raw)?;

    let mut reasons: Vec<String> = Vec::new();
    let mut meta = json!({
        "bvid": view.bvid,
        "aid": view.aid,
        "cid": page.cid,
        "page": page.page,
        "pageCount": view.pages.len(),
        "part": page.part,
        "stage": {"view": view_from},
    });

    let description = view.desc.trim();
    let (text, text_from) = if description.is_empty() || description == "-" {
        (view.title.trim(), "title")
    } else {
        (description, "description")
    };
    meta["textFrom"] = text_from.into();
    let title = if page.page > 1 {
        format!("{} P{} {}", view.title, page.page, page.part)
            .trim()
            .to_owned()
    } else {
        view.title.clone()
    };

    let mut output = FetchOutput {
        identity: Some(identity),
        kind: ItemKind::Video,
        title: Some(title),
        author: Some(Author {
            name: view.owner.name.clone(),
            platform_id: Some(view.owner.mid.to_string()),
            url: Some(format!("https://space.bilibili.com/{}", view.owner.mid)),
        }),
        published_at: jiff::Timestamp::from_second(view.pubdate)
            .ok()
            .filter(|_| view.pubdate > 0)
            .map(Rfc3339::from_timestamp),
        text: Some(text.to_owned()),
        text_format: Some(TextFormat::Plain),
        html: None,
        images: Vec::new(),
        media: Vec::new(),
        platform_tags: (!view.tname.is_empty())
            .then(|| view.tname.clone())
            .into_iter()
            .collect(),
        comments: None,
        completeness: Completeness::Partial,
        missing: Vec::new(),
        missing_reason: None,
        evidence: Evidence {
            fetched_at,
            access: Access::PublicWeb,
            site: None,
            upstream: None,
            status: Some(200),
        },
        credentials_update: None,
    };

    // 这个视频本来没有的部分：没有的不算缺（协议 §3.7）。
    // 文字有描述或标题就有；封面看 `pic`；音视频流看 playurl 的回答
    let mut absent: BTreeSet<OutputKind> = BTreeSet::new();
    if text.is_empty() {
        absent.insert(OutputKind::Text);
    }
    if view.pic.trim().is_empty() {
        absent.insert(OutputKind::Images);
    } else if wanted.contains(&OutputKind::Images) {
        match cover(api, out, &view.pic) {
            Ok(path) => output.images.push(path),
            Err(e) => reasons.push(soft(e, "封面")?),
        }
    }

    let want_video = wanted.contains(&OutputKind::Video);
    if wanted.contains(&OutputKind::Audio) {
        let (play, streams_from) = match from_page.play {
            Some(play) => (Ok(play), PAGE),
            None => (playurl(api, &view, page), API),
        };
        match play
            .and_then(|play| streams(api, out, play, page, want_video, &mut output, &mut absent))
        {
            Ok(streams_meta) => {
                meta["streams"] = streams_meta;
                meta["stage"]["streams"] = streams_from.into();
            }
            Err(e) => reasons.push(soft(e, "音视频流")?),
        }
    }

    write(
        out,
        "meta.json",
        &serde_json::to_vec_pretty(&meta)
            .map_err(|e| CallFailure::new(ErrorCode::Internal, format!("meta.json: {e}")))?,
    )?;

    let obtained = output.obtained();
    // 缺的 = 这个视频有、想要、又没交来的
    output.missing = wanted
        .iter()
        .filter(|kind| !absent.contains(kind) && !obtained.contains(kind))
        .copied()
        .collect();
    output.completeness = if output.missing.is_empty() {
        Completeness::Full
    } else {
        Completeness::Partial
    };
    if !output.missing.is_empty() {
        output.missing_reason = Some(reasons.join("；"));
    }
    Ok(output)
}

/// 可选的一步出错时：硬错误（网络、风控、限流、登录过期）原样报出，
/// 其余的把原因记进 `missingReason`。
fn soft(err: ApiError, what: &str) -> Result<String, CallFailure> {
    if err.is_hard() {
        return Err(err.into());
    }
    Ok(format!("{what}：{err}"))
}

fn rel(path: &str) -> Result<RelPath, CallFailure> {
    RelPath::try_from(path).map_err(|e| CallFailure::new(ErrorCode::Internal, e))
}

fn write(out: &Path, name: &str, bytes: &[u8]) -> Result<(), CallFailure> {
    let dest = out.join(name);
    let io = |source| ApiError::Io {
        path: name.to_owned(),
        source,
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    std::fs::write(&dest, bytes).map_err(io)?;
    Ok(())
}

fn cover(api: &Bilibili, out: &Path, pic: &str) -> Result<RelPath, ApiError> {
    if pic.is_empty() {
        return Err(ApiError::Shape("the video has no cover".into()));
    }
    let url = api.endpoints().media_url(pic)?;
    let ext = url
        .path()
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| matches!(e.as_str(), "jpg" | "jpeg" | "png" | "webp" | "gif" | "avif"))
        .unwrap_or_else(|| "jpg".into());
    let name = format!("images/cover.{ext}");
    step("封面");
    api.download(&url, &out.join(&name), COVER_BYTES, COVER_TIMEOUT, "封面")?;
    RelPath::try_from(name).map_err(ApiError::Shape)
}

/// `meta.json` 的 `stage` 里写的两种来处：视频页，或不签名的接口。
const PAGE: &str = "video-page";
const API: &str = "api";

/// 视频页里读到的两半：视频信息（`__INITIAL_STATE__.videoData`，和 `view` 接口的 `data`
/// 同样的字段，带着它的原始 JSON）和流地址（`__playinfo__.data`，和 playurl 的 `data` 同样的字段，
/// 只在有 DASH 时算读到）。读不到的那一半是 `None`，由接口补。
struct FromPage {
    view: Option<(View, Vec<u8>)>,
    play: Option<Play>,
}

/// 照 yt-dlp `yt_dlp/extractor/bilibili.py` 的 `BiliBiliIE._real_extract`（723-753、829-834 行）的先后：
/// 不带 cookie 取视频页（分 P 带 `?p=N`），读 `window.__INITIAL_STATE__` 和 `window.__playinfo__`。
/// 页面带 `window._riskdata_` 的 `v_voucher` 是风控的验证要求，回 `blocked`（照 api.rs 里接口带
/// `v_voucher` 的认法记成 -352）。页面取不到（跳走、404、太大）时两半都是 `None`；
/// 网络、风控、限流这些要停下来的照原样报出。
fn video_page(api: &Bilibili, video: &VideoRef) -> Result<FromPage, ApiError> {
    let path = format!("video/{}/", video.bvid);
    let part = video.page.to_string();
    let query: &[(&str, &str)] = if video.page > 1 {
        &[("p", part.as_str())]
    } else {
        &[]
    };
    let body = match api.page(&path, query) {
        Ok(body) => body,
        Err(e) if !e.is_hard() => {
            return Ok(FromPage {
                view: None,
                play: None,
            });
        }
        Err(e) => return Err(e),
    };
    let html = String::from_utf8_lossy(&body);
    if script_json(&html, "_riskdata_").is_some_and(|risk| !risk["v_voucher"].is_null()) {
        return Err(ApiError::Code {
            code: -352,
            message: "risk control on the video page".into(),
        }
        .answered(&body));
    }
    let view = match script_json(&html, "__INITIAL_STATE__")
        .as_ref()
        .and_then(|state| state.get("videoData"))
    {
        Some(data) => match View::deserialize(data) {
            Ok(view) => Some((
                view,
                serde_json::to_vec_pretty(data)
                    .map_err(|e| ApiError::Shape(format!("videoData: {e}")))?,
            )),
            Err(_) => None,
        },
        None => None,
    };
    let play = script_json(&html, "__playinfo__")
        .as_ref()
        .and_then(|info| info.get("data"))
        .and_then(|data| Play::deserialize(data).ok())
        .filter(|play| play.dash.is_some());
    Ok(FromPage { view, play })
}

/// 页面脚本里 `window.<name> =` 后面的那一个 JSON 值（照 yt-dlp `_search_json` 的读法：
/// 只读到这个值结束，后面的脚本不管）。没有、或读不出是 `None`：这一半改由接口取。
fn script_json(html: &str, name: &str) -> Option<Value> {
    let marker = format!("window.{name}");
    let mut rest = html;
    while let Some(at) = rest.find(&marker) {
        rest = &rest[at + marker.len()..];
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        return serde_json::Deserializer::from_str(value.trim_start())
            .into_iter::<Value>()
            .next()?
            .ok();
    }
    None
}

/// 页面里没有流地址时调不签名的 playurl。
fn playurl(api: &Bilibili, view: &View, page: &Page) -> Result<Play, ApiError> {
    let aid = view.aid.to_string();
    let cid = page.cid.to_string();
    step("音视频流地址");
    let reply = api.get(
        "x/player/playurl",
        &[
            ("avid", aid.as_str()),
            ("bvid", view.bvid.as_str()),
            ("cid", cid.as_str()),
            ("qn", "127"),
            ("otype", "json"),
            ("fnver", "0"),
            // 16 只要 DASH；挑流时取其中最低的
            ("fnval", "16"),
            ("fourk", "1"),
        ],
    )?;
    reply.data()
}

/// 取低画质的 DASH 流：视频和音频是两个文件，这里不合并（合并由宿主的转换步骤做）。
/// 两个都存了时带同一个 `group`（`cid:<cid>`），宿主据此知道它们是同一段的画面和音轨。
/// 视频没要时只写音频。DASH 里没有视频流或没有音频流的，记进 `absent`
/// （这个视频本来没有，不算缺）。返回写进 `meta.json` 的说明。
fn streams(
    api: &Bilibili,
    out: &Path,
    play: Play,
    page: &Page,
    want_video: bool,
    output: &mut FetchOutput,
    absent: &mut BTreeSet<OutputKind>,
) -> Result<Value, ApiError> {
    let dash = play.dash.ok_or_else(|| {
        ApiError::Shape(
            "playurl gave no DASH streams (an old video may only have a muxed FLV)".into(),
        )
    })?;
    let mut info = json!({
        "note": "video 和 audio 是分开的两个文件（DASH 流），要合并后才能带声音播放；这里没有合并",
    });
    let duration = (page.duration > 0.0).then_some(page.duration);
    let has_video = want_video && !dash.video.is_empty();
    let has_audio = !dash.audio.is_empty();
    if want_video && !has_video {
        absent.insert(OutputKind::Video);
    }
    if !has_audio {
        absent.insert(OutputKind::Audio);
    }
    let group = if has_video && has_audio {
        Some(MediaGroup::try_from(format!("cid:{}", page.cid)).map_err(ApiError::Shape)?)
    } else {
        None
    };

    if has_video {
        let stream = dash
            .video
            .iter()
            .filter(|s| s.codecid == CODEC_AVC)
            .min_by_key(|s| (s.id, s.bandwidth))
            .or_else(|| dash.video.iter().min_by_key(|s| (s.id, s.bandwidth)))
            .ok_or_else(|| ApiError::Shape("playurl has no video stream".into()))?;
        fetch_stream(
            api,
            out,
            "media/video.m4s",
            stream,
            VIDEO_BYTES,
            VIDEO_TIMEOUT,
            "视频",
        )?;
        info["video"] = json!({"quality": stream.id, "codecid": stream.codecid, "bandwidth": stream.bandwidth,
            "width": stream.width, "height": stream.height, "file": "media/video.m4s"});
        output.media.push(Media {
            path: rel("media/video.m4s").map_err(|f| ApiError::Shape(f.message))?,
            kind: MediaKind::Video,
            mime: Some("video/mp4".into()),
            lang: None,
            duration_sec: duration,
            group: group.clone(),
        });
    }

    let Some(stream) = dash.audio.iter().min_by_key(|s| (s.id, s.bandwidth)) else {
        return Ok(info);
    };
    fetch_stream(
        api,
        out,
        "media/audio.m4s",
        stream,
        AUDIO_BYTES,
        AUDIO_TIMEOUT,
        "音频",
    )?;
    info["audio"] =
        json!({"quality": stream.id, "bandwidth": stream.bandwidth, "file": "media/audio.m4s"});
    output.media.push(Media {
        path: rel("media/audio.m4s").map_err(|f| ApiError::Shape(f.message))?,
        kind: MediaKind::Audio,
        mime: Some("audio/mp4".into()),
        lang: None,
        duration_sec: duration,
        group,
    });
    Ok(info)
}

/// 主地址和备用地址里第一个在允许主机上、连之前的检查也过了的，下载到 `name`。
fn fetch_stream(
    api: &Bilibili,
    out: &Path,
    name: &str,
    stream: &DashStream,
    limit: u64,
    timeout: Duration,
    label: &str,
) -> Result<(), ApiError> {
    let mut first_error = None;
    let backups = stream.backup_url.iter().flatten().take(MAX_BACKUPS);
    let candidates = std::iter::once(&stream.base_url).chain(backups);
    for raw in candidates {
        match api.endpoints().media_url(raw) {
            Ok(url) => {
                step(label);
                match api.download(&url, &out.join(name), limit, timeout, label) {
                    Ok(_) => return Ok(()),
                    // 地址解析到内网之类、根本没连出去的，换下一个
                    Err(e @ ApiError::Refused(_)) => {
                        first_error.get_or_insert(e);
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| ApiError::Shape("the stream has no address".into())))
}
