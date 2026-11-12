//! 方法 `yutto`：全部离线。假的 yutto 和 ffmpeg 是几个 sh 脚本，放在一个临时目录里，测试只在这个目录里找程序；
//! 不运行真的 yutto，不向任何平台发请求。假 yutto 的行为由同一目录里的开关文件定：
//!
//! - `err.<模式>` 或 `err.all`：第一行是退出码，其余是要打印的文字（`__SESSDATA__`、`__PROXY__` 换成它拿到的值），
//!   `<模式>` 是 `video`、`audio`、`sub`。
//! - `hang.<模式>` 或 `hang.all`：一直不回。`big.<模式>`：写一个 2 KiB 的 `item.mp4`，`big.hang` 在时接着不退出。
//! - `no.subs`：不写字幕。`no.files`：什么文件都不写。`link.video`：画面文件是指向别处的符号链接。
//! - `version.text`：`--version` 打印它；`version.broken`：`--version` 退出 1。
//!
//! 每次调用的参数一行一个记进 `yutto.log`，调用之间隔一行 `---`；环境变量记进 `env.log`，当前目录、`HOME`、
//! `XDG_CONFIG_HOME` 记进 `where.log`，认证文件的权限和内容记进 `auth.mode`、`auth.seen`。
//! 夹具在 `tests/fixtures/yutto/`，都是手写的、没有测过（NOT MEASURED），照 yutto v2.3.1 的文档。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use usuyo_fetch_bilibili::api::{Bilibili, Endpoints, SITE};
use usuyo_fetch_bilibili::session::Session;
use usuyo_fetch_bilibili::tools::Search;
use usuyo_fetch_bilibili::yutto::{self, Context, Limits};
use usuyo_plugin::protocol::{
    Access, CallFailure, Completeness, ErrorCode, FetchHealth, FetchOutput, MediaKind,
    NotFoundReason, OutputKind,
};

const LINK: &str = "https://www.bilibili.com/video/BV17x411w7KC";
/// 带百分号的写法，好同时查原样、编码、解码三种写法。
const SESSDATA: &str = "s3cr3t%2Cvalue-abc";
const SESSDATA_DECODED: &str = "s3cr3t,value-abc";
const PROXY: &str = "http://proxyuser:proxypass@127.0.0.1:9";

const YUTTO: &str = r#"#!/bin/sh
D='__DIR__'
if [ "$1" = "--version" ]; then
  if [ -e "$D/version.broken" ]; then echo 'broken python' >&2; exit 1; fi
  if [ -e "$D/version.text" ]; then cat "$D/version.text"; else echo 'yutto 2.3.1'; fi
  exit 0
fi
{ for a in "$@"; do printf '%s\n' "$a"; done; echo '---'; } >> "$D/yutto.log"
{ env | sort; echo '---'; } >> "$D/env.log"
{ pwd; echo "$HOME"; echo "$XDG_CONFIG_HOME"; } >> "$D/where.log"
shift
MODE=; NOSUB=; OUT=; AUTH=; PROXY=
while [ $# -gt 0 ]; do
  case "$1" in
    --video-only) MODE=video ;;
    --audio-only) MODE=audio ;;
    --subtitle-only) MODE=sub ;;
    --no-subtitle) NOSUB=1 ;;
    -d) OUT=$2; shift ;;
    --auth-file) AUTH=$2; shift ;;
    --proxy) PROXY=$2; shift ;;
  esac
  shift
done
ls -l "$AUTH" >> "$D/auth.mode"
cat "$AUTH" >> "$D/auth.seen"
SECRET=$(sed -n 's/^sessdata = "\(.*\)"$/\1/p' "$AUTH")
if [ -e "$D/hang.$MODE" ] || [ -e "$D/hang.all" ]; then exec sleep 30; fi
ERRF="$D/err.$MODE"; [ -e "$ERRF" ] || ERRF="$D/err.all"
if [ -e "$ERRF" ]; then
  CODE=$(head -n 1 "$ERRF")
  tail -n +2 "$ERRF" | sed "s|__SESSDATA__|$SECRET|g; s|__PROXY__|$PROXY|g" >&2
  exit "$CODE"
fi
mkdir -p "$OUT"
if [ -e "$D/no.files" ]; then exit 0; fi
if [ -e "$D/big.$MODE" ]; then
  head -c 2048 /dev/zero > "$OUT/item.mp4"
  if [ -e "$D/big.hang" ]; then exec sleep 30; fi
  exit 0
fi
case "$MODE" in
  video)
    if [ -e "$D/link.video" ]; then ln -s /etc/hosts "$OUT/item.mp4"; else printf 'video' > "$OUT/item.mp4"; fi ;;
  audio) printf 'audio' > "$OUT/item.m4a" ;;
esac
if [ "$NOSUB" != 1 ] && [ ! -e "$D/no.subs" ]; then
  for l in zh-CN en; do cp "$D/item.srt" "$OUT/item.$l.srt"; done
  printf 'x' > "$OUT/item.bad lang.srt"
  printf 'x' > "$OUT/notes.txt"
fi
exit 0
"#;

const FFMPEG: &str = r#"#!/bin/sh
if [ "$1" = "-version" ]; then
  if [ -e '__DIR__/ffmpeg.broken' ]; then echo 'dyld error' >&2; exit 1; fi
  echo 'ffmpeg version 9.9-fake Copyright (c) the FFmpeg developers'; exit 0
fi
exit 0
"#;

struct Fakes {
    dir: tempfile::TempDir,
}

impl Fakes {
    fn new() -> Self {
        Self::with(&["yutto", "ffmpeg"])
    }

    fn with(names: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            let text = match *name {
                "yutto" => YUTTO,
                "ffmpeg" => FFMPEG,
                other => panic!("no fake for {other}"),
            };
            let path = dir.path().join(name);
            std::fs::write(
                &path,
                text.replace("__DIR__", &dir.path().display().to_string()),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/yutto");
        std::fs::copy(fixtures.join("item.srt"), dir.path().join("item.srt")).unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn set(&self, name: &str, content: &str) -> &Self {
        std::fs::write(self.path().join(name), content).unwrap();
        self
    }

    /// 把 `tests/fixtures/yutto/<name>` 当 `err.<mode>` 放进去。
    fn err(&self, mode: &str, fixture: &str) -> &Self {
        let text = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/yutto")
                .join(fixture),
        )
        .unwrap();
        self.set(&format!("err.{mode}"), &text)
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path().join(name)).unwrap_or_default()
    }

    /// 每次调用的参数。
    fn calls(&self) -> Vec<Vec<String>> {
        let mut calls = vec![Vec::new()];
        for line in self.read("yutto.log").lines() {
            if line == "---" {
                calls.push(Vec::new());
            } else {
                calls.last_mut().unwrap().push(line.to_owned());
            }
        }
        calls.pop();
        calls
    }
}

struct Setup {
    fakes: Fakes,
    tmp: tempfile::TempDir,
    out: tempfile::TempDir,
    ctx: Context,
}

fn setup() -> Setup {
    let fakes = Fakes::new();
    let tmp = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let ctx = Context {
        search: Search::dirs(vec![fakes.path().to_path_buf()]),
        proxy: None,
        tmp: tmp.path().to_path_buf(),
        limits: Limits::default(),
    };
    Setup {
        fakes,
        tmp,
        out,
        ctx,
    }
}

fn session() -> Session {
    let json = format!(
        r#"{{"storageState":{{"cookies":[
        {{"name":"SESSDATA","value":"{SESSDATA}","domain":".bilibili.com","path":"/","expires":4102444800}},
        {{"name":"DedeUserID","value":"12345","domain":".bilibili.com","path":"/","expires":-1}},
        {{"name":"bili_jct","value":"csrfvalue","domain":".bilibili.com","path":"/","expires":-1}}
        ],"origins":[]}}}}"#
    );
    Session::from_json(json.as_bytes(), 1_000).unwrap().unwrap()
}

fn api() -> Bilibili {
    Bilibili::new(Endpoints::production(), None)
}

fn run(s: &Setup, want: Option<&[OutputKind]>) -> Result<FetchOutput, CallFailure> {
    yutto::fetch(&api(), &s.ctx, Some(&session()), LINK, s.out.path(), want)
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).unwrap().next().is_none()
}

fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// 交出去的文字里不许有登录值的任何一种写法。
fn assert_no_secret(label: &str, text: &str) {
    for secret in [SESSDATA, SESSDATA_DECODED, "s3cr3t%2cvalue-abc"] {
        assert!(!text.contains(secret), "{label} contains the login value");
    }
}

fn kinds(output: &FetchOutput) -> Vec<(MediaKind, String)> {
    output
        .media
        .iter()
        .map(|m| (m.kind, m.path.as_str().to_owned()))
        .collect()
}

// ---- 好的一遍

#[test]
fn video_audio_and_subtitles_through_two_calls() {
    let s = setup();
    let output = run(&s, None).unwrap();
    assert_eq!(output.completeness, Completeness::Full);
    assert!(output.missing.is_empty() && output.missing_reason.is_none());
    assert_eq!(
        kinds(&output),
        [
            (MediaKind::Video, "media/video.mp4".to_owned()),
            (MediaKind::Audio, "media/audio.m4a".to_owned()),
            (MediaKind::Subtitle, "media/subtitle.en.srt".to_owned()),
            (MediaKind::Subtitle, "media/subtitle.zh-CN.srt".to_owned()),
        ]
    );
    // 画面和声音是一对，同一个 group；字幕不带
    let groups: Vec<_> = output
        .media
        .iter()
        .map(|m| m.group.as_ref().map(|g| g.as_str().to_owned()))
        .collect();
    assert_eq!(
        groups,
        [
            Some("yutto:BV17x411w7KC:p1".into()),
            Some("yutto:BV17x411w7KC:p1".into()),
            None,
            None
        ]
    );
    assert_eq!(output.media[0].mime.as_deref(), Some("video/mp4"));
    assert_eq!(output.media[1].mime.as_deref(), Some("audio/mp4"));
    assert_eq!(output.media[2].lang.as_deref(), Some("en"));
    assert_eq!(output.media[3].lang.as_deref(), Some("zh-CN"));
    for m in &output.media {
        assert!(s.out.path().join(m.path.as_str()).is_file());
    }
    assert_eq!(
        std::fs::read(s.out.path().join("media/video.mp4")).unwrap(),
        b"video"
    );
    // 没有被引用的东西不留在输出目录里：只有 media/
    let top: Vec<_> = std::fs::read_dir(s.out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(top, ["media"]);

    let evidence = &output.evidence;
    assert_eq!(evidence.access, Access::SessionApi);
    assert_eq!(evidence.site.as_deref(), Some(SITE));
    let upstream = evidence.upstream.as_ref().unwrap();
    assert_eq!(
        (upstream.name.as_str(), upstream.version.as_str()),
        ("yutto", "2.3.1")
    );
    assert!(output.title.is_none() && output.text.is_none());

    // 画面、声音各一次；字幕跟在第一次里
    let calls = s.fakes.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0][0], LINK);
    assert!(calls[0].contains(&"--video-only".to_owned()));
    assert!(!calls[0].contains(&"--no-subtitle".to_owned()));
    assert!(calls[1].contains(&"--audio-only".to_owned()));
    assert!(calls[1].contains(&"--no-subtitle".to_owned()));
    for call in &calls {
        for flag in [
            "--no-danmaku",
            "--no-cover",
            "--no-chapter-info",
            "--no-progress",
            "--login-strict",
        ] {
            assert!(call.contains(&flag.to_owned()), "{flag}");
        }
        assert_eq!(value_after(call, "-tp"), Some("item"));
        assert_eq!(value_after(call, "--auth-profile"), Some("default"));
        assert_eq!(value_after(call, "--proxy"), Some("no"));
        assert_eq!(
            value_after(call, "--ffmpeg-path").map(Path::new),
            Some(s.fakes.path().join("ffmpeg").as_path())
        );
        // 链接是插件拼的，不会被当成选项
        assert!(!call[0].starts_with('-'));
    }
    // 一次调用的输出目录各自独立
    assert_ne!(
        value_after(&calls[0], "-d"),
        value_after(&calls[1], "-d"),
        "each call has its own folder"
    );
    // 调用结束：工作目录整个没了，认证文件也没了
    assert!(is_empty_dir(s.tmp.path()));
}

#[test]
fn the_part_of_a_video_comes_from_the_link() {
    let s = setup();
    yutto::fetch(
        &api(),
        &s.ctx,
        Some(&session()),
        "https://www.bilibili.com/video/av170001?p=2",
        s.out.path(),
        Some(&[OutputKind::Video]),
    )
    .unwrap();
    let calls = s.fakes.calls();
    assert_eq!(
        calls[0][0],
        "https://www.bilibili.com/video/BV17x411w7KC?p=2"
    );
}

#[test]
fn only_the_asked_outputs_are_fetched() {
    let s = setup();
    let output = run(&s, Some(&[OutputKind::Subtitle])).unwrap();
    assert_eq!(output.completeness, Completeness::Full);
    assert_eq!(
        kinds(&output),
        [
            (MediaKind::Subtitle, "media/subtitle.en.srt".to_owned()),
            (MediaKind::Subtitle, "media/subtitle.zh-CN.srt".to_owned())
        ]
    );
    let calls = s.fakes.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(&"--subtitle-only".to_owned()));

    let s = setup();
    let output = run(&s, Some(&[OutputKind::Audio, OutputKind::Subtitle])).unwrap();
    assert_eq!(output.media.len(), 3);
    assert!(output.media.iter().all(|m| m.group.is_none()));
    let calls = s.fakes.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(&"--audio-only".to_owned()));
    assert!(!calls[0].contains(&"--no-subtitle".to_owned()));

    let s = setup();
    let output = run(&s, Some(&[OutputKind::Video])).unwrap();
    assert_eq!(output.media.len(), 1);
    assert!(s.fakes.calls()[0].contains(&"--no-subtitle".to_owned()));

    // 这个插件给不了的输出
    let s = setup();
    let err = run(&s, Some(&[OutputKind::Text])).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgs);
    assert!(s.fakes.calls().is_empty());
}

#[test]
fn a_video_without_subtitles_is_still_complete() {
    let s = setup();
    s.fakes.set("no.subs", "");
    let output = run(&s, None).unwrap();
    assert_eq!(output.completeness, Completeness::Full);
    assert_eq!(output.media.len(), 2);
    assert!(output.missing.is_empty());
}

#[test]
fn the_environment_is_cleared_and_private() {
    let s = setup();
    let mut ctx = s.ctx.clone();
    ctx.proxy = Some(PROXY.into());
    yutto::fetch(
        &api(),
        &ctx,
        Some(&session()),
        LINK,
        s.out.path(),
        Some(&[OutputKind::Video]),
    )
    .unwrap();
    let env = s.fakes.read("env.log");
    let names: Vec<&str> = env
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    for name in names {
        assert!(
            !name.contains("PROXY") && !name.contains("COOKIE") && !name.contains("USUYO"),
            "{name}"
        );
    }
    assert_no_secret("the environment", &env);
    // 当前目录、HOME、XDG_CONFIG_HOME 都在这次调用的工作目录里：读不到你自己的 yutto 配置
    let where_log = s.fakes.read("where.log");
    // 调用结束工作目录就没了，只能比路径的写法；macOS 的临时目录在 /var 和 /private/var 两种写法之间
    let root = |p: &Path| {
        p.to_str()
            .unwrap()
            .trim_start_matches("/private")
            .to_owned()
    };
    let tmp_root = root(s.tmp.path());
    let lines: Vec<&str> = where_log.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in lines {
        assert!(root(Path::new(line)).starts_with(&tmp_root), "{line}");
    }
    // 代理照宿主给的原样交给 --proxy
    assert_eq!(value_after(&s.fakes.calls()[0], "--proxy"), Some(PROXY));
}

#[test]
fn proxies_are_passed_or_refused() {
    let s = setup();
    let mut ctx = s.ctx.clone();
    ctx.proxy = Some("socks5://127.0.0.1:1080".into());
    yutto::fetch(
        &api(),
        &ctx,
        Some(&session()),
        LINK,
        s.out.path(),
        Some(&[OutputKind::Video]),
    )
    .unwrap();
    assert_eq!(
        value_after(&s.fakes.calls()[0], "--proxy"),
        Some("socks5h://127.0.0.1:1080")
    );

    // 写坏了的代理回错，不改成直连，也不把值写进错误
    let s = setup();
    let mut ctx = s.ctx.clone();
    ctx.proxy = Some("ftp://user:hunter2@example.test".into());
    let err = yutto::fetch(&api(), &ctx, Some(&session()), LINK, s.out.path(), None).unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
    assert!(!serde_json::to_string(&err).unwrap().contains("hunter2"));
    assert!(s.fakes.calls().is_empty());
}

// ---- 登录值不外泄

#[test]
fn the_login_value_reaches_only_a_private_auth_file() {
    let s = setup();
    run(&s, None).unwrap();
    let argv = s.fakes.read("yutto.log");
    assert_no_secret("the arguments", &argv);
    assert_no_secret("the environment", &s.fakes.read("env.log"));
    // 认证文件：yutto 看到它的时候权限是 0600，内容是 yutto 要的那种 toml，只有 SESSDATA（没有 bili_jct）
    let modes = s.fakes.read("auth.mode");
    assert_eq!(modes.lines().count(), 2);
    for line in modes.lines() {
        assert!(line.starts_with("-rw-------"), "{line}");
    }
    let seen = s.fakes.read("auth.seen");
    assert!(seen.contains(&format!("[profiles.default]\nsessdata = \"{SESSDATA}\"")));
    assert!(!seen.contains("csrfvalue") && !seen.contains("bili_jct"));
    // 用完就没了
    assert!(is_empty_dir(s.tmp.path()));
}

#[test]
fn the_login_value_is_scrubbed_from_every_error() {
    for (fixture, code) in [
        ("err-not-login.txt", ErrorCode::LoginRequired),
        ("err-traceback.txt", ErrorCode::UpstreamChanged),
        ("err-retry.txt", ErrorCode::Network),
    ] {
        let s = setup();
        s.fakes.err("all", fixture);
        let mut ctx = s.ctx.clone();
        ctx.proxy = Some(PROXY.into());
        let err =
            yutto::fetch(&api(), &ctx, Some(&session()), LINK, s.out.path(), None).unwrap_err();
        assert_eq!(err.code, code, "{fixture}");
        let json = serde_json::to_string(&err).unwrap();
        assert_no_secret(fixture, &json);
        assert!(!json.contains("proxypass"), "{fixture}");
        // 日志里本来有的话，替换成了占位，不是整句丢掉
        if fixture != "err-retry.txt" {
            assert!(
                err.detail
                    .as_deref()
                    .is_some_and(|d| d.contains("[removed]"))
            );
        }
        assert!(is_empty_dir(s.tmp.path()), "{fixture}");
        assert!(is_empty_dir(s.out.path()), "{fixture}");
    }
}

#[test]
fn the_binary_keeps_the_login_value_out_of_stdout_and_stderr() {
    let s = setup();
    let creds = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        creds.path(),
        format!(
            r#"{{"storageState":{{"cookies":[{{"name":"SESSDATA","value":"{SESSDATA}","domain":".bilibili.com","path":"/","expires":4102444800}}]}}}}"#
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_usuyo-fetch-bilibili");
    let call = |fail: bool| {
        let _ = std::fs::remove_file(s.fakes.path().join("err.all"));
        if fail {
            s.fakes.err("all", "err-not-login.txt");
        }
        let output = Command::new(bin)
            .args(["fetch", LINK, "--method", "yutto", "--out"])
            .arg(s.out.path())
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", s.fakes.path().display()),
            )
            .env("TMPDIR", s.tmp.path())
            .env("USUYO_CREDENTIALS", creds.path())
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    };
    let (code, stdout, stderr) = call(false);
    assert_eq!(code, 0, "{stderr}");
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["identity"]["contentId"], "BV17x411w7KC");
    assert_eq!(json["completeness"], "full");
    assert_eq!(json["evidence"]["access"], "session-api");
    assert_eq!(json["evidence"]["site"], SITE);
    assert_no_secret("stdout", &stdout);
    assert_no_secret("stderr", &stderr);
    // 宿主要的进度事件：一步一步报
    assert!(
        stderr.contains("@usuyo ") && stderr.contains("yutto 取画面"),
        "{stderr}"
    );

    let (code, stdout, stderr) = call(true);
    assert_eq!(code, 1);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["error"]["code"], "login-required");
    assert_eq!(json["error"]["site"], SITE);
    assert_no_secret("stdout", &stdout);
    assert_no_secret("stderr", &stderr);
    assert!(is_empty_dir(s.tmp.path()));
}

#[test]
fn without_a_login_nothing_is_started() {
    let s = setup();
    let err = yutto::fetch(&api(), &s.ctx, None, LINK, s.out.path(), None).unwrap_err();
    assert_eq!(err.code, ErrorCode::LoginRequired);
    assert_eq!(err.site.as_deref(), Some(SITE));
    assert!(s.fakes.calls().is_empty());
}

#[test]
fn the_binary_without_credentials_says_login_required() {
    let s = setup();
    let output = Command::new(env!("CARGO_BIN_EXE_usuyo-fetch-bilibili"))
        .args(["fetch", LINK, "--method", "yutto", "--out"])
        .arg(s.out.path())
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", s.fakes.path().display()),
        )
        .env("TMPDIR", s.tmp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["error"]["code"], "login-required");
}

// ---- 程序没装

#[test]
fn a_missing_program_is_dependency_missing_with_the_install_sentence() {
    for (have, needs, hint) in [
        (
            &["ffmpeg"][..],
            vec!["runtime:yutto"],
            "uv tool install yutto",
        ),
        (&["yutto"][..], vec!["runtime:ffmpeg"], "ffmpeg"),
        (
            &[][..],
            vec!["runtime:yutto", "runtime:ffmpeg"],
            "uv tool install yutto",
        ),
    ] {
        let fakes = Fakes::with(have);
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let ctx = Context {
            search: Search::dirs(vec![fakes.path().to_path_buf()]),
            proxy: None,
            tmp: tmp.path().to_path_buf(),
            limits: Limits::default(),
        };
        let err = yutto::fetch(&api(), &ctx, Some(&session()), LINK, out.path(), None).unwrap_err();
        assert_eq!(err.code, ErrorCode::DependencyMissing, "{have:?}");
        let got: Vec<String> = err.needs.iter().map(|n| format!("{n:?}")).collect();
        for n in needs {
            assert!(got.iter().any(|g| g.contains(n)), "{n} in {got:?}");
        }
        assert!(err.hint.as_deref().unwrap().contains(hint), "{err:?}");
        assert!(is_empty_dir(tmp.path()));
    }
}

#[test]
fn a_link_that_is_not_a_bilibili_video_starts_nothing() {
    let s = setup();
    let err = yutto::fetch(
        &api(),
        &s.ctx,
        Some(&session()),
        "https://example.com/video/BV17x411w7KC",
        s.out.path(),
        None,
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsupportedInput);
    assert!(s.fakes.calls().is_empty());
}

// ---- yutto 的错误回答

#[test]
fn yuttos_exit_codes_become_error_codes() {
    let rows: [(&str, ErrorCode, Option<NotFoundReason>); 7] = [
        ("err-not-login.txt", ErrorCode::LoginRequired, None),
        (
            "err-vip.txt",
            ErrorCode::NotFound,
            Some(NotFoundReason::Paid),
        ),
        (
            "err-region.txt",
            ErrorCode::NotFound,
            Some(NotFoundReason::Region),
        ),
        (
            "err-deleted.txt",
            ErrorCode::NotFound,
            Some(NotFoundReason::Deleted),
        ),
        ("err-unsupported.txt", ErrorCode::UnsupportedInput, None),
        ("err-412.txt", ErrorCode::Blocked, None),
        ("err-429.txt", ErrorCode::RateLimited, None),
    ];
    for (fixture, code, reason) in rows {
        let s = setup();
        s.fakes.err("all", fixture);
        let err = run(&s, None).unwrap_err();
        assert_eq!((err.code, err.reason), (code, reason), "{fixture}");
        assert!(err.hint.is_some(), "{fixture}");
        // 平台原样回答的末尾进 detail
        assert!(
            err.detail.as_deref().is_some_and(|d| d.contains("ERROR")),
            "{fixture}"
        );
        // 出错就停：后面的调用不再发
        assert_eq!(s.fakes.calls().len(), 1, "{fixture}");
        assert!(is_empty_dir(s.out.path()), "{fixture}");
        assert!(is_empty_dir(s.tmp.path()), "{fixture}");
    }
    for (fixture, code) in [
        ("err-retry.txt", ErrorCode::Network),
        ("err-ffmpeg.txt", ErrorCode::Internal),
        ("err-traceback.txt", ErrorCode::UpstreamChanged),
    ] {
        let s = setup();
        s.fakes.err("all", fixture);
        let err = run(&s, None).unwrap_err();
        assert_eq!(err.code, code, "{fixture}");
    }
    // 认不出的退出，提示说怎么更新 yutto
    let s = setup();
    s.fakes.err("all", "err-traceback.txt");
    let err = run(&s, None).unwrap_err();
    assert!(
        err.hint
            .as_deref()
            .unwrap()
            .contains("uv tool upgrade yutto")
    );
}

#[test]
fn one_part_failing_softly_leaves_the_rest() {
    // 画面那次合成失败（ffmpeg）：声音和字幕照取，缺的写在 missing 里
    let s = setup();
    s.fakes.err("video", "err-ffmpeg.txt");
    let output = run(&s, None).unwrap();
    assert_eq!(output.completeness, Completeness::Partial);
    // 字幕本来跟在画面那次里，它失败了字幕也没有
    assert_eq!(output.missing, [OutputKind::Video, OutputKind::Subtitle]);
    assert!(output.missing_reason.as_deref().unwrap().contains("ffmpeg"));
    assert_eq!(
        kinds(&output)[0],
        (MediaKind::Audio, "media/audio.m4a".to_owned())
    );
    assert!(output.media.iter().all(|m| m.group.is_none()));
}

#[test]
fn yutto_finishing_without_files_is_upstream_changed() {
    let s = setup();
    s.fakes.set("no.files", "");
    let err = run(&s, None).unwrap_err();
    assert_eq!(err.code, ErrorCode::UpstreamChanged);
    assert!(err.hint.is_some());
}

#[test]
fn only_plain_files_with_the_expected_names_are_taken() {
    // 符号链接不当成画面；名字不合的字幕和别的文件不要
    let s = setup();
    s.fakes.set("link.video", "");
    let output = run(&s, Some(&[OutputKind::Video])).unwrap_err();
    assert_eq!(output.code, ErrorCode::UpstreamChanged);

    let s = setup();
    let output = run(&s, None).unwrap();
    let names: Vec<_> = kinds(&output).into_iter().map(|(_, p)| p).collect();
    assert!(
        names
            .iter()
            .all(|n| !n.contains("bad") && !n.contains("notes"))
    );
}

// ---- 时限和大小

#[test]
fn a_hanging_yutto_is_stopped_at_the_deadline() {
    let mut s = setup();
    s.fakes.set("hang.all", "");
    s.ctx.limits.budget = Duration::from_millis(1500);
    let started = std::time::Instant::now();
    let err = run(&s, None).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(err.code, ErrorCode::Network);
    assert!(err.hint.is_some());
    assert!(is_empty_dir(s.tmp.path()));
}

#[test]
fn a_late_hang_keeps_what_was_already_fetched() {
    let mut s = setup();
    s.fakes.set("hang.audio", "");
    s.ctx.limits.budget = Duration::from_millis(2500);
    let output = run(&s, None).unwrap();
    assert_eq!(output.completeness, Completeness::Partial);
    assert_eq!(output.missing, [OutputKind::Audio]);
    assert!(output.missing_reason.as_deref().unwrap().contains("时限"));
    assert_eq!(output.media[0].kind, MediaKind::Video);
}

#[test]
fn a_file_over_the_limit_is_not_kept() {
    // 一直写、不退出的：量目录时发现，杀掉
    let mut s = setup();
    s.ctx.limits.max_file_bytes = 1024;
    s.fakes.set("big.video", "").set("big.hang", "");
    let output = run(&s, None).unwrap();
    assert_eq!(output.missing, [OutputKind::Video, OutputKind::Subtitle]);
    assert!(output.missing_reason.as_deref().unwrap().contains("大于"));
    assert_eq!(output.media.len(), 1);
    assert_eq!(output.media[0].kind, MediaKind::Audio);
    assert!(is_empty_dir(s.tmp.path()));

    // 写完就退出的：事后查大小
    let mut s = setup();
    s.ctx.limits.max_file_bytes = 1024;
    s.fakes.set("big.video", "");
    let output = run(&s, None).unwrap();
    assert_eq!(output.missing, [OutputKind::Video, OutputKind::Subtitle]);
    assert!(!s.out.path().join("media/video.mp4").exists());
}

// ---- health

fn health(fakes: &Fakes) -> FetchHealth {
    yutto::add_checks(
        FetchHealth::new("0.1.0"),
        &Search::dirs(vec![fakes.path().to_path_buf()]),
    )
}

fn check<'a>(h: &'a FetchHealth, name: &str) -> &'a usuyo_plugin::protocol::MethodCheck {
    h.checks.iter().find(|c| c.name == name).unwrap()
}

#[test]
fn health_finds_both_programs() {
    let h = health(&Fakes::new());
    assert!(h.ready);
    for name in ["yutto", "ffmpeg"] {
        let c = check(&h, name);
        assert!(c.ok, "{c:?}");
        assert_eq!(c.methods, ["yutto"]);
    }
    assert_eq!(
        check(&h, "yutto").message.as_deref(),
        Some("找到 yutto 2.3.1")
    );
    assert_eq!(
        check(&h, "ffmpeg").message.as_deref(),
        Some("找到 ffmpeg 9.9-fake")
    );
    let versions: Vec<_> = h
        .upstream
        .iter()
        .map(|u| (u.name.as_str(), u.version.as_str()))
        .collect();
    assert_eq!(versions, [("yutto", "2.3.1"), ("ffmpeg", "9.9-fake")]);
}

#[test]
fn health_blocks_only_the_yutto_method_when_a_program_is_missing() {
    let h = health(&Fakes::with(&["ffmpeg"]));
    // 只挡方法 yutto，插件本身仍然 ready：别的方法用不着它们
    assert!(h.ready);
    let c = check(&h, "yutto");
    assert!(!c.ok);
    assert_eq!(c.methods, ["yutto"]);
    let message = c.message.as_deref().unwrap();
    assert!(message.starts_with("没找到 yutto：请先安装，例如在终端运行 uv tool install yutto"));
    assert!(check(&h, "ffmpeg").ok);

    let h = health(&Fakes::with(&["yutto"]));
    let c = check(&h, "ffmpeg");
    assert!(!c.ok && c.methods == ["yutto"]);
    assert!(
        c.message
            .as_deref()
            .unwrap()
            .contains("没找到 ffmpeg（yutto 合并画面和声音要用）")
    );
}

#[test]
fn health_reports_an_old_or_broken_yutto() {
    let f = Fakes::new();
    f.set("version.text", "yutto 2.2.9\n");
    let c = health(&f);
    let c = check(&c, "yutto");
    assert!(!c.ok);
    assert!(c.message.as_deref().unwrap().contains("太旧"));
    assert!(
        c.message
            .as_deref()
            .unwrap()
            .contains("uv tool upgrade yutto")
    );

    // 比最低版本新的、带后缀的都过
    for text in ["yutto 2.3.1", "yutto 2.10.0.dev3", "yutto 3.0.0"] {
        let f = Fakes::new();
        f.set("version.text", text);
        assert!(check(&health(&f), "yutto").ok, "{text}");
    }

    let f = Fakes::new();
    f.set("version.broken", "");
    let h = health(&f);
    let c = check(&h, "yutto");
    assert!(!c.ok && c.methods == ["yutto"]);
    assert!(c.message.as_deref().unwrap().contains("运行不了"));

    let f = Fakes::new();
    f.set("ffmpeg.broken", "");
    assert!(!check(&health(&f), "ffmpeg").ok);
}

#[test]
fn the_binarys_health_names_the_method_it_blocks() {
    let fakes = Fakes::with(&["ffmpeg"]);
    // PATH 只有假的目录和系统目录；后备目录里若恰好有真的 yutto 就不是这一条要测的
    if Search::dirs(
        usuyo_fetch_bilibili::tools::FALLBACK_DIRS
            .map(PathBuf::from)
            .into(),
    )
    .find("yutto")
    .is_some()
    {
        return;
    }
    let output = Command::new(env!("CARGO_BIN_EXE_usuyo-fetch-bilibili"))
        .arg("health")
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", fakes.path().display()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ready"], true);
    let checks = json["checks"].as_array().unwrap();
    let yutto = checks.iter().find(|c| c["name"] == "yutto").unwrap();
    assert_eq!(yutto["ok"], false);
    assert_eq!(yutto["methods"], serde_json::json!(["yutto"]));
}
