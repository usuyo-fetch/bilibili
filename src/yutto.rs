//! 方法 `yutto`：包你自己装的 yutto（GPL-3.0，独立进程，插件不带、不下载、不链接它），按一条视频链接取
//! 高画质的画面、声音和字幕（协议 §9.1）。要你在 usuyo 里登录的 B站，宿主只在这次调用里交来凭据。
//!
//! 调法（yutto 2.3.1 的文档和源码，`docs/guide/cli/` 和 `src/yutto/`；没有对着真的 yutto 跑过，
//! 只对过假程序）：每次一个子进程，链接是插件自己拼的规范链接（不会以 `-` 开头）。
//!
//! ```text
//! yutto <链接> --video-only | --audio-only | --subtitle-only
//!   --no-danmaku --no-cover --no-chapter-info [--no-subtitle] --no-progress --no-color --login-strict
//!   -d <调用目录>/out --tmp-dir <调用目录>/tmp -tp item
//!   --auth-file <工作目录>/auth.toml --auth-profile default --proxy <宿主的代理或 no> --ffmpeg-path <ffmpeg>
//! ```
//!
//! - 登录：认证文件是 TOML（`[profiles.default]` 下 `sessdata`），0600，写在这次调用自己的工作目录里，
//!   用完马上删（出错也删）。`SESSDATA` 不进参数、环境、日志、错误和 `detail`：yutto 的输出和我们自己的
//!   错误文字交出去之前，都先把它（原样、百分号编码、解码后三种写法）和代理地址替换掉。
//! - 环境：先清空，`HOME`、`XDG_CONFIG_HOME` 指向工作目录里的空目录，工作目录里没有 `yutto.toml`，所以
//!   yutto 读不到你自己的配置（里面可能有别的认证或代理）。
//! - 画面、声音分两次（`--video-only`、`--audio-only`）：yutto 每次都用 ffmpeg 把流封成 `item.mp4`、
//!   `item.m4a`，字幕写成 `item.<语言>.srt`，名字由 `-tp item` 固定，不用网络上来的标题。字幕跟在第一次
//!   取画面或声音的调用里一起写；只要字幕时单独一次 `--subtitle-only`。
//! - 上限：每个文件至多 [`Limits::max_file_bytes`]（调用目录里任何一个文件超过就杀掉 yutto），整次
//!   调用至多 [`Limits::budget`]；yutto 的输出只留开头 [`LOG_KEEP`] 字节。
//! - `--login-strict`：登录失效时 yutto 报错退出，不悄悄用游客身份取低画质。

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use usuyo_plugin_kit::http::Proxy;
use usuyo_plugin_kit::protocol::{
    Access, CallFailure, Completeness, ErrorCode, Evidence, FetchHealth, FetchOutput, ItemKind,
    Media, MediaGroup, MediaKind, NotFoundReason, OutputKind, PluginEvent, RelPath, Rfc3339,
    UpstreamVersion,
};

use crate::api::{Bilibili, SITE};
use crate::canonical::{identity, locate};
use crate::session::Session;
use crate::tools::{self, Flow, Run, Search};

/// 方法 id，和清单一致。
pub const METHOD: &str = "yutto";

/// 能给的输出，和清单的 `outputs` 一致。
pub const OUTPUTS: [OutputKind; 3] = [OutputKind::Video, OutputKind::Audio, OutputKind::Subtitle];

/// 文档（`docs/guide/cli/auth.md`）写到的最低版本：`--auth-file`、`--login-strict`、`--ffmpeg-path`。
pub const MIN_VERSION: (u32, u32, u32) = (2, 3, 1);

/// yutto 输出的文件名（不含后缀）：`-tp item`。
const ITEM: &str = "item";
/// yutto 的输出只留开头这么多字节。
const LOG_KEEP: usize = 64 << 10;
/// 字幕至多存几种语言。
const MAX_SUBTITLES: usize = 8;
/// 每个程序回版本至多等多久；`health` 的总时长是 10 秒，两个同时问。
const VERSION_TIMEOUT: Duration = Duration::from_secs(4);
/// 报进度的间隔：宿主 120 秒没收到新的 `progress` 就杀掉整次调用。
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// 一次 `fetch` 的时限和长度上限。
#[derive(Debug, Clone)]
pub struct Limits {
    /// 整次调用，比清单的 `timeout: 1h` 短，留出放文件的时间。
    pub budget: Duration,
    /// 每个文件至多多少字节。
    pub max_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            budget: Duration::from_secs(55 * 60),
            max_file_bytes: 2 << 30,
        }
    }
}

/// 一次调用的环境：去哪找程序、走哪条路、工作目录放哪。
#[derive(Debug, Clone)]
pub struct Context {
    pub search: Search,
    /// 宿主的 `USUYO_PROXY`。
    pub proxy: Option<String>,
    /// 工作目录建在这里（宿主给的 `TMPDIR`，调用结束整个丢掉）。
    pub tmp: PathBuf,
    pub limits: Limits,
}

impl Context {
    pub fn from_env() -> Self {
        Self {
            search: Search::from_env(),
            proxy: std::env::var("USUYO_PROXY").ok(),
            tmp: std::env::temp_dir(),
            limits: Limits::default(),
        }
    }
}

// ---- health

/// 装或更新一个程序的命令，按系统写。
fn install_command(verb: &str, name: &str) -> String {
    match (name, cfg!(target_os = "macos")) {
        ("yutto", _) => format!("uv tool {verb} yutto"),
        (_, true) => format!("brew {verb} {name}"),
        _ => format!("用系统的包管理器安装 {name}"),
    }
}

/// 一个程序找没找到、版本是多少。
enum Probe {
    Missing,
    Broken(String),
    Version(String),
}

fn probe(search: &Search, name: &str, flag: &str, word: usize) -> Probe {
    let Some(path) = search.find(name) else {
        return Probe::Missing;
    };
    match tools::version(&path, flag, VERSION_TIMEOUT) {
        // `yutto 2.3.1`、`ffmpeg version 9.0.1 Copyright …`
        Ok(line) => Probe::Version(
            line.split_whitespace()
                .nth(word)
                .unwrap_or(&line)
                .to_owned(),
        ),
        Err(why) => Probe::Broken(why),
    }
}

/// `yutto 2.3.1`、`2.3.1.dev3` 里的前三段数字。
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.').map(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// 给用户看的一句话：找到了是哪个版本，没找到要装什么、怎么装。
fn message(probe: &Probe, name: &str, why: &str) -> String {
    match probe {
        Probe::Version(v) => format!("找到 {name} {v}"),
        Probe::Missing => format!(
            "没找到 {name}{why}：请先安装，例如在终端运行 {}",
            install_command("install", name)
        ),
        Probe::Broken(reason) => format!(
            "找到了 {name}，但它运行不了（{reason}）：请重新安装，例如在终端运行 {}",
            install_command(
                if name == "yutto" {
                    "upgrade"
                } else {
                    "reinstall"
                },
                name
            )
        ),
    }
}

/// 在 `health` 的回答里加 yutto 和 ffmpeg 两项检查：没过时只挡方法 `yutto`。只看本机，不碰平台。
pub fn add_checks(mut health: FetchHealth, search: &Search) -> FetchHealth {
    let (yutto, ffmpeg) = std::thread::scope(|s| {
        let a = s.spawn(|| probe(search, "yutto", "--version", 1));
        let b = s.spawn(|| probe(search, "ffmpeg", "-version", 2));
        (
            a.join()
                .unwrap_or(Probe::Broken("the probe panicked".into())),
            b.join()
                .unwrap_or(Probe::Broken("the probe panicked".into())),
        )
    });
    for (name, p) in [("yutto", &yutto), ("ffmpeg", &ffmpeg)] {
        if let Probe::Version(v) = p {
            health.upstream.push(UpstreamVersion {
                name: name.into(),
                version: v.clone(),
            });
        }
    }
    // 版本太旧：当作没过，说怎么更新
    let too_old = match &yutto {
        Probe::Version(v) => parse_version(v).is_some_and(|v| v < MIN_VERSION),
        _ => false,
    };
    let yutto_message = match &yutto {
        Probe::Version(v) if too_old => format!(
            "yutto {v} 太旧，这个插件按 {}.{}.{} 以上写的：请更新，例如在终端运行 {}",
            MIN_VERSION.0,
            MIN_VERSION.1,
            MIN_VERSION.2,
            install_command("upgrade", "yutto")
        ),
        other => message(other, "yutto", ""),
    };
    health = health.check(
        "yutto",
        matches!(yutto, Probe::Version(_)) && !too_old,
        Some(yutto_message),
        &[METHOD],
    );
    health.check(
        "ffmpeg",
        matches!(ffmpeg, Probe::Version(_)),
        Some(message(&ffmpeg, "ffmpeg", "（yutto 合并画面和声音要用）")),
        &[METHOD],
    )
}

// ---- 凭据不外泄

/// 不许出现在任何交出去的文字里的值。
struct Secrets(Vec<String>);

impl Secrets {
    fn new(session: Option<&Session>, proxy: Option<&str>) -> Self {
        let mut values = Vec::new();
        if let Some(s) = session {
            let raw = s.sessdata();
            values.extend([raw.to_owned(), percent_decode(raw), percent_encode(raw)]);
        }
        // 代理地址里可能带用户名和密码
        values.extend(proxy.map(str::to_owned));
        values.retain(|v| v.len() >= 3);
        Self(values)
    }

    fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for v in &self.0 {
            out = out.replace(v.as_str(), "[removed]");
        }
        out
    }

    fn scrub_failure(&self, mut f: CallFailure) -> CallFailure {
        f.message = self.scrub(&f.message);
        f.hint = f.hint.map(|h| self.scrub(&h));
        f.detail = f.detail.map(|d| Box::new(self.scrub(&d)));
        f
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---- 工作目录

/// `${TMP}` 下这次调用的工作目录（0700），丢掉时连同里面的东西删掉。
struct WorkDir(PathBuf);

impl WorkDir {
    fn new(tmp: &Path) -> Result<Self, CallFailure> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = tmp.join(format!("yutto-{}-{nanos}", std::process::id()));
        make_dir(&dir).map_err(|e| {
            io_failure(
                &format!("cannot make a work directory in {}", tmp.display()),
                &e,
            )
        })?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

fn io_failure(what: &str, e: &std::io::Error) -> CallFailure {
    CallFailure::new(ErrorCode::Internal, format!("{what}: {e}"))
}

/// yutto 的认证文件：0600，建的时候就带着权限；丢掉（包括出错提前返回）时删掉。
struct AuthFile(PathBuf);

impl AuthFile {
    fn write(dir: &Path, sessdata: &str) -> Result<Self, CallFailure> {
        let path = dir.join("auth.toml");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|e| io_failure("cannot create the auth file", &e))?;
        // 记下路径再写：写到一半出错也由 Drop 删掉
        let guard = Self(path);
        // 值里不会有 `"` 和 `\`（`Session` 只收可见 ASCII，没有引号、反斜杠），照 TOML 的规矩仍然转义
        let escaped = sessdata.replace('\\', "\\\\").replace('"', "\\\"");
        file.write_all(format!("[profiles.default]\nsessdata = \"{escaped}\"\n").as_bytes())
            .map_err(|e| io_failure("cannot write the auth file", &e))?;
        Ok(guard)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for AuthFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---- fetch

/// 一次 yutto 调用取什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Video,
    Audio,
    SubtitleOnly,
}

struct Plan {
    part: Part,
    /// 这次调用顺带写字幕。
    with_subtitles: bool,
}

/// 要的几样分成几次调用：字幕跟在第一次取画面或声音的调用里，都没有时单独一次。
fn plan(wanted: &BTreeSet<OutputKind>) -> Vec<Plan> {
    let mut subtitles = wanted.contains(&OutputKind::Subtitle);
    let mut calls = Vec::new();
    for (kind, part) in [
        (OutputKind::Video, Part::Video),
        (OutputKind::Audio, Part::Audio),
    ] {
        if wanted.contains(&kind) {
            calls.push(Plan {
                part,
                with_subtitles: std::mem::take(&mut subtitles),
            });
        }
    }
    if subtitles {
        calls.push(Plan {
            part: Part::SubtitleOnly,
            with_subtitles: true,
        });
    }
    calls
}

/// 一次调用交出的文件，已经在调用目录里。
#[derive(Default)]
struct Found {
    video: Option<(PathBuf, String)>,
    audio: Option<(PathBuf, String)>,
    /// (语言, 文件, 后缀)
    subtitles: Vec<(String, PathBuf, String)>,
}

enum Outcome {
    Done,
    TooLarge,
    TimedOut,
    Failed(CallFailure),
}

pub fn fetch(
    api: &Bilibili,
    ctx: &Context,
    session: Option<&Session>,
    input: &str,
    out: &Path,
    want: Option<&[OutputKind]>,
) -> Result<FetchOutput, CallFailure> {
    let secrets = Secrets::new(session, ctx.proxy.as_deref());
    fetch_inner(api, ctx, session, &secrets, input, out, want).map_err(|f| secrets.scrub_failure(f))
}

fn fetch_inner(
    api: &Bilibili,
    ctx: &Context,
    session: Option<&Session>,
    secrets: &Secrets,
    input: &str,
    out: &Path,
    want: Option<&[OutputKind]>,
) -> Result<FetchOutput, CallFailure> {
    let deadline = Instant::now() + ctx.limits.budget;
    let wanted: BTreeSet<OutputKind> =
        want.map_or_else(|| OUTPUTS.into(), |w| w.iter().copied().collect());
    if let Some(other) = wanted.iter().find(|w| !OUTPUTS.contains(w)) {
        return Err(CallFailure::new(
            ErrorCode::InvalidArgs,
            format!("cannot give `{other}`"),
        ));
    }
    let programs = programs(&ctx.search)?;
    let Some(session) = session else {
        return Err(CallFailure {
            hint: Some("yutto 要用你在 usuyo 里登录的 B站：请先在设置里登录 B站".into()),
            site: Some(SITE.into()),
            ..CallFailure::new(
                ErrorCode::LoginRequired,
                "no credentials for www.bilibili.com",
            )
        });
    };
    let proxy = proxy_arg(ctx.proxy.as_deref())?;
    let found = locate(api, input)?.ok_or_else(|| {
        CallFailure::new(ErrorCode::UnsupportedInput, "not a bilibili video link")
    })?;
    let video = found.video;
    let url = video.canonical_url();
    let identity = identity(&video)?;
    let version = tools::version(&programs.yutto, "--version", VERSION_TIMEOUT)
        .ok()
        .and_then(|l| l.split_whitespace().nth(1).map(str::to_owned));
    let fetched_at = Rfc3339::from_timestamp(jiff::Timestamp::now());

    let work = WorkDir::new(&ctx.tmp)?;
    let auth = AuthFile::write(work.path(), session.sessdata())?;
    for dir in ["home/.config", "home/.cache", "tmp"] {
        make_dir(&work.path().join(dir))
            .map_err(|e| io_failure("cannot make a work folder", &e))?;
    }

    let mut media: Vec<Media> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    let mut absent: BTreeSet<OutputKind> = BTreeSet::new();
    let mut first_failure: Option<CallFailure> = None;
    let mut timed_out = false;
    let mut got = Found::default();
    for (n, call) in plan(&wanted).iter().enumerate() {
        if Instant::now() >= deadline {
            reasons.push("超过了这次调用的时限，后面几样没有取".into());
            timed_out = true;
            break;
        }
        let dir = work.path().join(format!("call-{n}"));
        make_dir(&dir.join("out")).map_err(|e| io_failure("cannot make a work folder", &e))?;
        make_dir(&dir.join("tmp")).map_err(|e| io_failure("cannot make a work folder", &e))?;
        let args = yutto_args(call, &url, &dir, auth.path(), &proxy, &programs.ffmpeg);
        step(match call.part {
            Part::Video => "yutto 取画面",
            Part::Audio => "yutto 取声音",
            Part::SubtitleOnly => "yutto 取字幕",
        });
        let (outcome, log) = run_yutto(
            ctx,
            &programs,
            work.path(),
            &dir,
            args,
            deadline,
            version.as_deref(),
        )?;
        let files = collect(&dir.join("out"));
        match outcome {
            Outcome::Failed(failure) => {
                let failure = failure.with_detail(tail(&secrets.scrub(&log), 4096));
                if stops(failure.code) {
                    return Err(failure);
                }
                reasons.push(
                    failure
                        .hint
                        .clone()
                        .unwrap_or_else(|| failure.message.clone()),
                );
                first_failure.get_or_insert(failure);
                continue;
            }
            Outcome::TooLarge => {
                reasons.push(format!(
                    "文件大于 {}，没有保存",
                    size_text(ctx.limits.max_file_bytes)
                ));
                continue;
            }
            Outcome::TimedOut => {
                reasons.push("yutto 超过了这次调用的时限".into());
                timed_out = true;
                continue;
            }
            Outcome::Done => {}
        }
        let mut oversized = false;
        for path in files
            .video
            .iter()
            .chain(files.audio.iter())
            .map(|(p, _)| p)
            .chain(files.subtitles.iter().map(|(_, p, _)| p))
        {
            oversized |= std::fs::metadata(path).is_ok_and(|m| m.len() > ctx.limits.max_file_bytes);
        }
        if oversized {
            reasons.push(format!(
                "文件大于 {}，没有保存",
                size_text(ctx.limits.max_file_bytes)
            ));
            continue;
        }
        // 正常退出又没写出文件：yutto 的输出和这个插件认的对不上，多半是 yutto 改了
        let mut no_file = |what: &str| {
            reasons.push(format!("yutto 没有交出{what}文件"));
            let failure = CallFailure {
                hint: Some(update_hint(version.as_deref())),
                ..CallFailure::new(
                    ErrorCode::UpstreamChanged,
                    format!("yutto exited normally but wrote no {what} file"),
                )
            }
            .with_detail(tail(&secrets.scrub(&log), 4096));
            first_failure.get_or_insert(failure);
        };
        match call.part {
            Part::Video => match files.video {
                Some(f) => got.video = Some(f),
                None => no_file("画面"),
            },
            Part::Audio => match files.audio {
                Some(f) => got.audio = Some(f),
                None => no_file("声音"),
            },
            Part::SubtitleOnly => {}
        }
        if call.with_subtitles {
            if files.subtitles.is_empty() {
                // 正常退出又没写字幕：这个视频本来没有
                absent.insert(OutputKind::Subtitle);
            } else {
                got.subtitles = files.subtitles;
            }
        }
    }
    // 先交出文件，再记哪些拿到了
    let group = (got.video.is_some() && got.audio.is_some())
        .then(|| MediaGroup::try_from(format!("yutto:{}:p{}", video.bvid, video.page)).ok())
        .flatten();
    if let Some((path, ext)) = got.video {
        media.push(Media {
            path: place(out, &path, &format!("media/video.{ext}"))?,
            kind: MediaKind::Video,
            mime: video_mime(&ext),
            lang: None,
            duration_sec: None,
            group: group.clone(),
        });
    }
    if let Some((path, ext)) = got.audio {
        media.push(Media {
            path: place(out, &path, &format!("media/audio.{ext}"))?,
            kind: MediaKind::Audio,
            mime: audio_mime(&ext),
            lang: None,
            duration_sec: None,
            group,
        });
    }
    got.subtitles.sort();
    if got.subtitles.len() > MAX_SUBTITLES {
        got.subtitles.truncate(MAX_SUBTITLES);
        reasons.push(format!("字幕只保存了前 {MAX_SUBTITLES} 种语言"));
    }
    for (lang, path, ext) in got.subtitles {
        media.push(Media {
            path: place(out, &path, &format!("media/subtitle.{lang}.{ext}"))?,
            kind: MediaKind::Subtitle,
            mime: Some("application/x-subrip".into()),
            lang: Some(lang),
            duration_sec: None,
            group: None,
        });
    }

    let mut output = FetchOutput {
        identity: Some(identity),
        kind: ItemKind::Video,
        title: None,
        author: None,
        published_at: None,
        text: None,
        text_format: None,
        html: None,
        images: Vec::new(),
        media,
        platform_tags: Vec::new(),
        comments: None,
        completeness: Completeness::Partial,
        missing: Vec::new(),
        missing_reason: None,
        evidence: Evidence {
            fetched_at,
            access: Access::SessionApi,
            site: Some(SITE.into()),
            upstream: version.map(|v| UpstreamVersion {
                name: "yutto".into(),
                version: v,
            }),
            status: None,
        },
        credentials_update: None,
    };
    let obtained = output.obtained();
    output.missing = wanted
        .iter()
        .filter(|k| !absent.contains(k) && !obtained.contains(k))
        .copied()
        .collect();
    // 一样都没取到、又不是「本来就没有」：超时是网络问题，有失败就交出第一个失败，
    // 什么原因都没记下就是 yutto 退出正常却没写文件；只是文件太大的照常交出（缺的写在 `missing` 里）
    if obtained.is_empty() && !output.missing.is_empty() {
        if timed_out {
            return Err(CallFailure {
                hint: Some("yutto 太久没有取完；过一会儿再试".into()),
                ..CallFailure::new(
                    ErrorCode::Network,
                    "yutto did not finish within the time limit",
                )
            });
        }
        if let Some(failure) = first_failure {
            return Err(failure);
        }
        if reasons.is_empty() {
            let version = output
                .evidence
                .upstream
                .as_ref()
                .map(|u| u.version.as_str());
            return Err(CallFailure {
                hint: Some(update_hint(version)),
                ..CallFailure::new(
                    ErrorCode::UpstreamChanged,
                    "yutto exited normally but wrote none of the expected files",
                )
            });
        }
    }
    output.completeness = if output.missing.is_empty() {
        Completeness::Full
    } else {
        Completeness::Partial
    };
    if !output.missing.is_empty() {
        let mut seen = BTreeSet::new();
        reasons.retain(|r| seen.insert(r.clone()));
        output.missing_reason = Some(secrets.scrub(&reasons.join("；")));
    }
    Ok(output)
}

/// 找到的程序。
struct Programs {
    yutto: PathBuf,
    ffmpeg: PathBuf,
}

fn programs(search: &Search) -> Result<Programs, CallFailure> {
    let yutto = search.find("yutto");
    let ffmpeg = search.find("ffmpeg");
    match (yutto, ffmpeg) {
        (Some(yutto), Some(ffmpeg)) => Ok(Programs { yutto, ffmpeg }),
        (yutto, ffmpeg) => {
            let missing: Vec<&str> = [("yutto", yutto.is_none()), ("ffmpeg", ffmpeg.is_none())]
                .into_iter()
                .filter_map(|(n, gone)| gone.then_some(n))
                .collect();
            let how: Vec<String> = missing
                .iter()
                .map(|n| install_command("install", n))
                .collect();
            let mut f = CallFailure {
                hint: Some(format!(
                    "没找到 {}：请先安装，例如在终端运行 {}",
                    missing.join("、"),
                    how.join("；")
                )),
                ..CallFailure::new(
                    ErrorCode::DependencyMissing,
                    format!("not found: {}", missing.join(", ")),
                )
            };
            f.needs = missing
                .iter()
                .map(|n| need(&format!("runtime:{n}")))
                .collect();
            Err(f)
        }
    }
}

fn need(text: &str) -> usuyo_plugin_kit::protocol::Need {
    usuyo_plugin_kit::protocol::Need::try_from(text.to_owned()).expect("constant need")
}

/// 宿主的 `USUYO_PROXY`（`http://`、`socks5://`、`socks5h://`）换成给 `--proxy` 的值；没有时是 `no`（直连）：
/// 默认的 `auto` 会让 yutto 去读系统的代理设置。写坏了的回错，不改成直连。
/// plugin-kit 把 `socks5://` 当成由代理查名字，yutto 要写成 `socks5h://` 才这样，所以改写。
fn proxy_arg(value: Option<&str>) -> Result<String, CallFailure> {
    let Some(text) = value.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok("no".into());
    };
    Proxy::parse(text).map_err(|e| {
        CallFailure::new(
            ErrorCode::Internal,
            format!("USUYO_PROXY is not a usable proxy: {e}"),
        )
    })?;
    Ok(match text.strip_prefix("socks5://") {
        Some(rest) => format!("socks5h://{rest}"),
        None => text.to_owned(),
    })
}

fn yutto_args(
    call: &Plan,
    url: &str,
    dir: &Path,
    auth: &Path,
    proxy: &str,
    ffmpeg: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![url.to_owned()];
    args.push(
        match call.part {
            Part::Video => "--video-only",
            Part::Audio => "--audio-only",
            Part::SubtitleOnly => "--subtitle-only",
        }
        .into(),
    );
    if !call.with_subtitles {
        args.push("--no-subtitle".into());
    }
    args.extend(
        [
            "--no-danmaku",
            "--no-cover",
            "--no-chapter-info",
            "--no-progress",
            "--no-color",
            "--login-strict",
            "-tp",
            ITEM,
            "--auth-profile",
            "default",
            "-d",
            &dir.join("out").display().to_string(),
            "--tmp-dir",
            &dir.join("tmp").display().to_string(),
            "--auth-file",
            &auth.display().to_string(),
            "--proxy",
            proxy,
            "--ffmpeg-path",
            &ffmpeg.display().to_string(),
        ]
        .map(str::to_owned),
    );
    args
}

/// 起一次 yutto。返回结果和它的输出（两路合在一起，只留开头）。
fn run_yutto(
    ctx: &Context,
    programs: &Programs,
    work: &Path,
    dir: &Path,
    args: Vec<String>,
    deadline: Instant,
    version: Option<&str>,
) -> Result<(Outcome, String), CallFailure> {
    let mut env = tools::base_env();
    let home = work.join("home");
    for (key, value) in [
        ("HOME", home.clone()),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_CACHE_HOME", home.join(".cache")),
        ("TMPDIR", work.join("tmp")),
        ("TMP", work.join("tmp")),
        ("TEMP", work.join("tmp")),
    ] {
        env.retain(|(k, _)| k != key);
        env.push((key.into(), value.display().to_string()));
    }
    let mut log = String::new();
    let mut too_large = false;
    let mut last_report: Option<Instant> = None;
    let finished = tools::run(
        Run {
            program: &programs.yutto,
            args,
            env,
            // 工作目录里没有 yutto.toml：它先找当前目录下的这个文件
            cwd: Some(dir),
            deadline,
            keep_stdout: 0,
        },
        |_, line| {
            if log.len() < LOG_KEEP {
                let room = LOG_KEEP - log.len();
                let mut end = line.len().min(room);
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                log.push_str(&line[..end]);
                log.push('\n');
            }
            Flow::Go
        },
        || {
            let (total, biggest) = measure(dir);
            if biggest > ctx.limits.max_file_bytes {
                too_large = true;
                return Flow::Stop;
            }
            if last_report.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY) {
                last_report = Some(Instant::now());
                progress(total);
            }
            Flow::Go
        },
    )
    .map_err(|e| CallFailure::new(ErrorCode::Internal, e.to_string()))?;
    let outcome = if too_large {
        Outcome::TooLarge
    } else if finished.timed_out() {
        Outcome::TimedOut
    } else if finished.success() {
        Outcome::Done
    } else {
        let code = finished.status.and_then(|s| s.code());
        Outcome::Failed(failure(code, &log, version))
    };
    Ok((outcome, log))
}

/// 调用目录里所有文件的字节数之和，和最大的那个文件。
fn measure(dir: &Path) -> (u64, u64) {
    let mut total = 0;
    let mut biggest = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
                biggest = biggest.max(meta.len());
            }
        }
    }
    (total, biggest)
}

/// 调用目录的 `out/` 里 yutto 写完的文件：`item.<后缀>`（画面、声音）、`item.<语言>.srt`（字幕）。
/// 只认普通文件，名字照 `-tp item`；别的不要。
fn collect(out_dir: &Path) -> Found {
    let mut found = Found::default();
    let Ok(entries) = std::fs::read_dir(out_dir) else {
        return found;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let Some(rest) = name.strip_prefix(ITEM).and_then(|r| r.strip_prefix('.')) else {
            continue;
        };
        let path = out_dir.join(&name);
        match rest.split_once('.') {
            None if matches!(rest, "mp4" | "mkv" | "mov") => {
                found.video.get_or_insert((path, rest.to_owned()));
            }
            None if matches!(rest, "m4a" | "aac" | "mp3" | "flac" | "mp4" | "mkv" | "mov") => {
                found.audio.get_or_insert((path, rest.to_owned()));
            }
            Some((lang, "srt")) if plain_lang(lang) => {
                found.subtitles.push((lang.to_owned(), path, "srt".into()));
            }
            _ => {}
        }
    }
    found
}

/// 语言名来自平台，会进输出文件名：只认字母、数字、`-`、`_`，1 到 32 字节。
fn plain_lang(lang: &str) -> bool {
    (1..=32).contains(&lang.len())
        && lang
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 把调用目录里的一个文件移进 `--out` 的 `name`。
fn place(out: &Path, from: &Path, name: &str) -> Result<RelPath, CallFailure> {
    let dest = out.join(name);
    let fail = |e: std::io::Error| {
        CallFailure::new(ErrorCode::Internal, format!("cannot place {name}: {e}"))
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    if std::fs::rename(from, &dest).is_err() {
        std::fs::copy(from, &dest).map_err(fail)?;
    }
    RelPath::try_from(name.to_owned()).map_err(|e| CallFailure::new(ErrorCode::Internal, e))
}

fn video_mime(ext: &str) -> Option<String> {
    Some(
        match ext {
            "mp4" => "video/mp4",
            "mkv" => "video/x-matroska",
            "mov" => "video/quicktime",
            _ => return None,
        }
        .into(),
    )
}

fn audio_mime(ext: &str) -> Option<String> {
    Some(
        match ext {
            "m4a" | "mp4" => "audio/mp4",
            "aac" => "audio/aac",
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "mkv" => "audio/x-matroska",
            "mov" => "audio/quicktime",
            _ => return None,
        }
        .into(),
    )
}

fn size_text(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{} GiB", bytes >> 30)
    } else {
        format!("{} MiB", bytes >> 20)
    }
}

/// 末尾至多 `max` 字节（yutto 把错误写在最后）。
fn tail(text: &str, max: usize) -> &str {
    let text = text.trim();
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn step(label: &str) {
    emit(PluginEvent::Progress {
        done: None,
        total: None,
        unit: None,
        message: Some(label.into()),
    });
}

fn progress(total: u64) {
    emit(PluginEvent::Progress {
        done: Some((total as f64 / 1e5).round() / 10.0),
        total: None,
        unit: Some("MB".into()),
        message: Some("yutto 下载中".into()),
    });
}

fn emit(event: PluginEvent) {
    eprintln!("{}", event.stderr_line());
}

// ---- yutto 的错误

/// 停下报错，不再取后面的、不交一部分：要换个时间、换条路、重新登录才取得到，或这条本来就取不到。
/// 剩下的（合成失败、认不出的退出）只记进 `missingReason`，别的几样照取。
fn stops(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::RateLimited
            | ErrorCode::Blocked
            | ErrorCode::Network
            | ErrorCode::LoginRequired
            | ErrorCode::DependencyMissing
            | ErrorCode::NotFound
            | ErrorCode::UnsupportedInput
    )
}

fn update_hint(version: Option<&str>) -> String {
    let now = version.map_or_else(String::new, |v| format!("（这台电脑上是 {v}）"));
    format!(
        "yutto{now}和这个插件对不上，可能是 B站改了或 yutto 太旧：请更新到最新版，例如在终端运行 {}",
        install_command("upgrade", "yutto")
    )
}

/// 照 yutto 的退出码（`src/yutto/exceptions.py`：10 HTTP 状态、11 没有权限、12 不支持的类型、13 参数错、
/// 14 链接错、15 没有这一话、16 重试用尽、17 找不到、18 没登录、19 解密、20 后处理、21 解析失败）
/// 和输出里的字样定错误码。字样没有对着真的 yutto 的输出核对过，认不出时落到 `upstream-changed`。
fn failure(code: Option<i32>, log: &str, version: Option<&str>) -> CallFailure {
    let text = log.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    let message = match code {
        Some(c) => format!("yutto exited with code {c}"),
        None => "yutto was stopped by a signal".to_owned(),
    };
    let with = |code: ErrorCode, hint: &str| CallFailure {
        hint: Some(hint.to_owned()),
        ..CallFailure::new(code, message.clone())
    };
    let not_found = |reason: Option<NotFoundReason>, hint: &str| CallFailure {
        reason,
        ..with(ErrorCode::NotFound, hint)
    };
    match code {
        Some(18) => CallFailure {
            site: Some(SITE.into()),
            ..with(
                ErrorCode::LoginRequired,
                "B站的登录已过期，请在设置里重新登录 B站",
            )
        },
        Some(11) if has(&["大会员", "付费", "充电", "购买", "vip"]) => {
            not_found(Some(NotFoundReason::Paid), "这条要大会员或付费才看得到")
        }
        Some(11) if has(&["地区", "区域", "港澳台", "region"]) => {
            not_found(Some(NotFoundReason::Region), "这条在你现在的地区看不到")
        }
        Some(11 | 15 | 17) if has(&["删除", "已失效", "deleted"]) => {
            not_found(Some(NotFoundReason::Deleted), "这条在 B站上已经看不到了")
        }
        Some(11 | 15 | 17) => not_found(None, "B站上找不到这条，或没有权限看"),
        Some(12 | 14) => with(
            ErrorCode::UnsupportedInput,
            "yutto 认不出这条链接，交给 B站的别的方法",
        ),
        Some(10) if has(&["412", "352", "风控"]) => with(
            ErrorCode::Blocked,
            "B站出现了风控提示；请稍后再试，也可以先在浏览器里打开 B站确认账号一切正常",
        ),
        Some(10) if has(&["429", "频繁", "-799", "-509"]) => {
            with(ErrorCode::RateLimited, "B站提示请求太频繁，请过一会儿再试")
        }
        Some(10) if has(&["404"]) => not_found(None, "B站上找不到这条"),
        Some(10 | 16) if has(&["500", "502", "503", "504", "timeout", "超时", "重试"]) => {
            with(ErrorCode::Network, "网络问题，没连上；过一会儿再试")
        }
        Some(16) => with(ErrorCode::Network, "网络问题，没连上；过一会儿再试"),
        Some(20) => with(
            ErrorCode::Internal,
            "ffmpeg 没能合成画面或声音：请检查 ffmpeg 能不能正常运行",
        ),
        _ => CallFailure {
            hint: Some(update_hint(version)),
            ..CallFailure::new(ErrorCode::UpstreamChanged, message)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("2.3.1"), Some((2, 3, 1)));
        assert_eq!(parse_version("2.10.0.dev3"), Some((2, 10, 0)));
        assert_eq!(parse_version("2.4"), Some((2, 4, 0)));
        assert_eq!(parse_version("x"), None);
    }

    #[test]
    fn percent_forms() {
        assert_eq!(percent_decode("a%2Cb%7E"), "a,b~");
        assert_eq!(percent_encode("a,b"), "a%2Cb");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn plan_puts_subtitles_in_the_first_call() {
        let set = |k: &[OutputKind]| k.iter().copied().collect::<BTreeSet<_>>();
        let all = plan(&set(&OUTPUTS));
        assert_eq!(all.len(), 2);
        assert!(all[0].with_subtitles && !all[1].with_subtitles);
        let audio_sub = plan(&set(&[OutputKind::Audio, OutputKind::Subtitle]));
        assert_eq!(audio_sub.len(), 1);
        assert_eq!(audio_sub[0].part, Part::Audio);
        let only_sub = plan(&set(&[OutputKind::Subtitle]));
        assert_eq!(only_sub[0].part, Part::SubtitleOnly);
        assert!(plan(&set(&[])).is_empty());
    }
}
