//! `fetch`：一个视频的元数据、封面、字幕，和按需要的低画质音视频流。
//!
//! 输出目录里的文件名都是固定的，不用网络上来的名字：
//! `view.json`（`view` 接口的原始响应）、`meta.json`、`images/cover.<ext>`、
//! `media/video.m4s`、`media/audio.m4s`、`media/subtitle.<lang>.vtt`。

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use usuyo_plugin_kit::protocol::{
    Access, Author, CallFailure, Completeness, ErrorCode, Evidence, FetchOutput, ItemKind, Media,
    MediaKind, OutputKind, RelPath, Rfc3339, TextFormat,
};

use crate::api::{ApiError, Bilibili, SITE, step};
use crate::canonical::{identity, locate};

/// 本插件能给的输出，和清单里的 `outputs` 一致。
pub const OUTPUTS: [OutputKind; 6] = [
    OutputKind::Text,
    OutputKind::Images,
    OutputKind::Video,
    OutputKind::Audio,
    OutputKind::Subtitle,
    OutputKind::Meta,
];

const COVER_BYTES: u64 = 20 << 20;
const COVER_TIMEOUT: Duration = Duration::from_secs(30);
const SUBTITLE_BYTES: u64 = 4 << 20;
const SUBTITLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SUBTITLES: usize = 8;
const MAX_CUES: usize = 20_000;
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

#[derive(Deserialize)]
struct PlayerInfo {
    subtitle: Option<SubtitleList>,
}

#[derive(Deserialize)]
struct SubtitleList {
    #[serde(default)]
    subtitles: Vec<SubtitleEntry>,
}

#[derive(Deserialize)]
struct SubtitleEntry {
    #[serde(default)]
    lan: String,
    #[serde(default)]
    subtitle_url: String,
}

#[derive(Deserialize)]
struct SubtitleFile {
    body: Vec<Cue>,
}

#[derive(Deserialize)]
struct Cue {
    from: f64,
    to: f64,
    content: String,
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
    step("视频信息");
    let reply = api.get("x/web-interface/view", &[("bvid", video.bvid.as_str())])?;
    let view: View = reply.data()?;
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
    write(out, "view.json", &reply.raw)?;

    let mut reasons: Vec<String> = Vec::new();
    let mut meta = json!({
        "bvid": view.bvid,
        "aid": view.aid,
        "cid": page.cid,
        "page": page.page,
        "pageCount": view.pages.len(),
        "part": page.part,
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
            access: if api.session().is_some() {
                Access::SessionApi
            } else {
                Access::PublicWeb
            },
            site: api.session().map(|_| SITE.to_owned()),
            upstream: None,
            status: Some(200),
        },
        credentials_update: None,
    };

    if wanted.contains(&OutputKind::Images) {
        match cover(api, out, &view.pic) {
            Ok(path) => output.images.push(path),
            Err(e) => reasons.push(soft(e, "封面")?),
        }
    }

    if wanted.contains(&OutputKind::Subtitle) {
        if api.session().is_none() {
            reasons.push("字幕需要登录 B站".into());
        } else {
            match subtitles(api, out, &view, page) {
                Ok(Subtitles { found, skipped }) => {
                    if !skipped.is_empty() {
                        meta["subtitleSkipped"] = skipped.clone().into();
                    }
                    if found.is_empty() {
                        reasons.push(if skipped.is_empty() {
                            "这个视频没有可用的字幕".into()
                        } else {
                            format!("字幕：{}", skipped.join("；"))
                        });
                    }
                    meta["subtitles"] = found
                        .iter()
                        .map(|(lang, path)| json!({"lang": lang, "file": path}))
                        .collect();
                    for (lang, path) in found {
                        output.media.push(Media {
                            path: rel(&path)?,
                            kind: MediaKind::Subtitle,
                            mime: Some("text/vtt".into()),
                            lang: Some(lang),
                            duration_sec: None,
                        });
                    }
                }
                Err(e) => reasons.push(soft(e, "字幕")?),
            }
        }
    }

    let want_video = wanted.contains(&OutputKind::Video);
    if wanted.contains(&OutputKind::Audio) {
        match streams(api, out, &view, page, want_video, &mut output) {
            Ok(streams_meta) => meta["streams"] = streams_meta,
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
    output.missing = wanted.difference(&obtained).copied().collect();
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

/// 字幕这一步的结果。
struct Subtitles {
    /// 写出的 `(语言, 相对路径)`。
    found: Vec<(String, String)>,
    /// 跳过的几条和原因。
    skipped: Vec<String>,
}

/// 取字幕，转成 WebVTT。返回写出的 `(语言, 相对路径)` 和跳过的几条的原因。
/// 每条字幕各自处理：一条不可用只跳过它，硬错误（网络、风控、登录过期）整个报出。
/// 只在有登录时调用。
fn subtitles(api: &Bilibili, out: &Path, view: &View, page: &Page) -> Result<Subtitles, ApiError> {
    let aid = view.aid.to_string();
    let cid = page.cid.to_string();
    step("字幕列表");
    let reply = api.get(
        "x/player/wbi/v2",
        &[
            ("aid", aid.as_str()),
            ("bvid", view.bvid.as_str()),
            ("cid", cid.as_str()),
        ],
    )?;
    let info: PlayerInfo = reply.data()?;
    let entries = info.subtitle.map(|s| s.subtitles).unwrap_or_default();
    let mut found: Vec<(String, String)> = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries
        .into_iter()
        .filter(|e| !e.subtitle_url.is_empty())
        .take(MAX_SUBTITLES)
    {
        let lang = lang_code(&entry.lan);
        step(&format!("字幕 {lang}"));
        let vtt = match subtitle_vtt(api, &entry) {
            Ok(vtt) => vtt,
            Err(e) if !e.is_hard() => {
                skipped.push(format!("{lang}：{e}"));
                continue;
            }
            Err(e) => return Err(e),
        };
        let mut name = format!("media/subtitle.{lang}.vtt");
        let mut n = 2;
        while found.iter().any(|(_, p)| *p == name) {
            name = format!("media/subtitle.{lang}-{n}.vtt");
            n += 1;
        }
        let io = |source| ApiError::Io {
            path: name.clone(),
            source,
        };
        std::fs::create_dir_all(out.join("media")).map_err(io)?;
        std::fs::write(out.join(&name), vtt).map_err(io)?;
        found.push((lang, name));
    }
    Ok(Subtitles { found, skipped })
}

fn subtitle_vtt(api: &Bilibili, entry: &SubtitleEntry) -> Result<String, ApiError> {
    let url = api.endpoints().media_url(&entry.subtitle_url)?;
    let body = api.get_bytes(&url, SUBTITLE_BYTES, SUBTITLE_TIMEOUT)?;
    let file: SubtitleFile = serde_json::from_slice(&body)
        .map_err(|e| ApiError::Shape(format!("subtitle file ({:?})", e.classify())))?;
    Ok(to_vtt(&file.body))
}

/// 字幕语言码：只留字母、数字、连字符，别的整个换成 `und`。
fn lang_code(lan: &str) -> String {
    let ok = !lan.is_empty()
        && lan.len() <= 16
        && lan.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok { lan.to_owned() } else { "und".into() }
}

fn vtt_time(seconds: f64) -> Option<String> {
    if !seconds.is_finite() || !(0.0..1e7).contains(&seconds) {
        return None;
    }
    let ms = (seconds * 1000.0).round() as u64;
    Some(format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    ))
}

fn to_vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues.iter().take(MAX_CUES) {
        let (Some(from), Some(to)) = (vtt_time(cue.from), vtt_time(cue.to)) else {
            continue;
        };
        // 空行会结束一个 cue，`-->` 会被当成时间行
        let text = cue
            .content
            .replace('\r', "")
            .replace("-->", "->")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            continue;
        }
        out.push_str(&format!("{from} --> {to}\n{text}\n\n"));
    }
    out
}

/// 取低画质的 DASH 流：视频和音频是两个文件，这里不合并（合并由宿主的转换步骤做）。
/// 视频没要时只写音频。返回写进 `meta.json` 的说明。
fn streams(
    api: &Bilibili,
    out: &Path,
    view: &View,
    page: &Page,
    want_video: bool,
    output: &mut FetchOutput,
) -> Result<Value, ApiError> {
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
    let play: Play = reply.data()?;
    let dash = play.dash.ok_or_else(|| {
        ApiError::Shape(
            "playurl gave no DASH streams (an old video may only have a muxed FLV)".into(),
        )
    })?;
    let mut info = json!({
        "note": "video 和 audio 是分开的两个文件（DASH 流），要合并后才能带声音播放；这里没有合并",
    });
    let duration = (page.duration > 0.0).then_some(page.duration);

    if want_video {
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
        });
    }

    let stream = dash
        .audio
        .iter()
        .min_by_key(|s| (s.id, s.bandwidth))
        .ok_or_else(|| ApiError::Shape("playurl has no audio stream".into()))?;
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
