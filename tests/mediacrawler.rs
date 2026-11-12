//! `mediacrawler` 方法：读你自己跑 MediaCrawler 留下的帖子文件，列成链接。全部离线：起插件进程时清空环境，
//! 代理指向一个关着的本机端口，万一哪条路径真的发了请求，只会失败，不会连到B站；文件太大一类的错误
//! 用小的上限直接调库函数，不必真造几百 MB。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use usuyo_fetch_bilibili::mediacrawler::{Limits, list};
use usuyo_plugin::protocol::{
    Access, Account, ErrorCode, ListOrder, ListPage, Login, PluginKind, PluginManifest,
};
use usuyo_plugin::{CallLimits, CallOptions, HostConfig, ListRequest, PluginHost, discover};

const BIN: &str = env!("CARGO_BIN_EXE_usuyo-fetch-bilibili");
const DEAD_PROXY: &str = "http://127.0.0.1:9";
/// 样例文件里的帖子，新的在前：地址、标题、作者、发布时间。最后一条文件里没有时间。
type Row<'a> = (&'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>);
const EXPECTED: [Row<'static>; 3] = [
    (
        "https://www.bilibili.com/video/BV1xx411c7mD",
        Some("磨豆子的粗细"),
        Some("咖***人"),
        Some("2026-10-15T08:30:00Z"),
    ),
    (
        "https://www.bilibili.com/video/BV17x411w7KC",
        Some("手冲咖啡：三段式注水"),
        Some("手***啡"),
        Some("2026-10-01T12:00:00Z"),
    ),
    (
        "https://www.bilibili.com/video/BV1xx411c7mS",
        Some("一篇没有发布时间的帖子"),
        Some("路***人"),
        None,
    ),
];

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mediacrawler")
            .join(name),
    )
    .unwrap()
}

fn plugin(args: &[&str], input: Option<&Path>) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .env("USUYO_PROTOCOL", "1")
        .env("USUYO_LOCALE", "zh-CN")
        .env("HTTP_PROXY", DEAD_PROXY)
        .env("HTTPS_PROXY", DEAD_PROXY)
        .env("ALL_PROXY", DEAD_PROXY)
        .args(args);
    if let Some(i) = input {
        cmd.env("USUYO_INPUT", i);
    }
    cmd.output().unwrap()
}

/// stdout 上只有一个 JSON 值。
fn envelope(out: &Output) -> Value {
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    assert_eq!(
        stdout.lines().count(),
        1,
        "stdout is one JSON value: {stdout}"
    );
    serde_json::from_str(&stdout).unwrap()
}

fn list_call(input: &Path) -> Output {
    plugin(
        &["list", "file:input", "--method", "mediacrawler"],
        Some(input),
    )
}

fn listed(input: &Path) -> Value {
    let out = list_call(input);
    assert!(out.status.success(), "{out:?}");
    envelope(&out)
}

/// 列失败：退出码 1，信封里的错误。
fn failure(input: &Path) -> Value {
    let out = list_call(input);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let value = envelope(&out);
    assert_eq!(value["ok"], false, "{value}");
    value["error"].clone()
}

fn file_in(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// 按名字写一个 zip；名字原样写进去，包括带 `..` 的。
fn zip_of(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, bytes) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

const POSTS: &str = "search_contents_2026-11-01.json";
const POSTS_JSONL: &str = "search_contents_2026-11-01.jsonl";
const COMMENTS: &str = "search_comments_2026-11-01.json";
const COMMENTS_JSONL: &str = "search_comments_2026-11-01.jsonl";
const CSV: &[u8] = b"\xef\xbb\xbfid,title\n1,invented\n";
const SQLITE: &[u8] = b"SQLite format 3\0invented";

fn assert_posts(value: &Value) {
    assert_eq!(value["ok"], true, "{value}");
    let mut page = value.clone();
    page.as_object_mut().unwrap().remove("ok");
    let parsed: ListPage = serde_json::from_value(page).unwrap();
    assert_eq!(parsed.next_cursor, None);
    assert_eq!(value["evidence"]["access"], "private-api");
    let items = value["items"].as_array().unwrap();
    let rows: Vec<Row> = items
        .iter()
        .map(|i| {
            (
                i["url"].as_str().unwrap(),
                i["title"].as_str(),
                i["author"]["name"].as_str(),
                i["publishedAt"].as_str(),
            )
        })
        .collect();
    assert_eq!(rows, EXPECTED);
    for item in items {
        // 只有链接：身份和平台插件别的方法给的一样；不带 ref、不带存下的时间
        assert_eq!(item["identity"]["platform"], "bilibili", "{item}");
        assert_eq!(item["identity"]["url"], item["url"], "{item}");
        for absent in ["ref", "actedAt", "action"] {
            assert!(item.get(absent).is_none(), "{absent} in {item}");
        }
        assert!(item["author"].get("platformId").is_none());
    }
    assert_eq!(items[0]["identity"]["contentId"], "BV1xx411c7mD");
    assert_eq!(items[1]["identity"]["contentId"], "BV17x411w7KC");
    assert_eq!(items[2]["identity"]["contentId"], "BV1xx411c7mS");
}

fn manifest() -> PluginManifest {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("usuyo-plugin.yaml"),
    )
    .unwrap();
    let parsed = PluginManifest::from_yaml(&text).unwrap();
    assert!(
        parsed.unknown_fields.is_empty(),
        "{:?}",
        parsed.unknown_fields
    );
    parsed.value
}

#[test]
fn the_manifest_declares_the_mediacrawler_method() {
    let m = manifest();
    let PluginKind::Fetch(spec) = &m.kind else {
        panic!("not a fetch plugin");
    };
    let method = spec.method("mediacrawler").unwrap();
    assert!(method.built && method.sites.is_empty() && method.visitors.is_empty());
    assert_eq!(
        (method.access, method.login, method.account),
        (Access::PrivateApi, Login::None, Account::None)
    );
    // private-api 这一层默认关着，用户在获取插件页开
    assert!(!method.on_by_default());
    let list = method.capabilities.list.as_ref().unwrap();
    // 新旧排的是发布时间，不是加入时间，不写 newest-first
    assert_eq!(list.order, ListOrder::Unordered);
    assert!(list.files.iter().any(|f| f == "*.zip") && list.reads.is_some());
    assert!(method.capabilities.fetch.is_none());
    let risk = method.risk.as_ref().unwrap();
    assert!(risk.text.contains("MediaCrawler") && risk.text.contains("不向任何平台发请求"));
    assert_eq!(
        risk.source.url,
        "https://github.com/NanmiCoder/MediaCrawler/issues/865"
    );
    // 读文件要放行 ${INPUT}
    assert!(
        std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("usuyo-plugin.yaml")
        )
        .unwrap()
        .contains("read: [\"${INPUT}\"]")
    );
}

#[test]
fn lists_posts_from_a_json_file_whatever_it_is_called() {
    let tmp = tempfile::tempdir().unwrap();
    let named = file_in(tmp.path(), POSTS, &fixture(POSTS));
    assert_posts(&listed(&named));
    // 认的是结构，不是名字
    let renamed = file_in(tmp.path(), "input.dat", &fixture(POSTS));
    assert_posts(&listed(&renamed));
}

#[test]
fn lists_the_same_posts_from_json_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let path = file_in(tmp.path(), POSTS_JSONL, &fixture(POSTS_JSONL));
    assert_posts(&listed(&path));
}

#[test]
fn lists_posts_from_a_zip_of_the_output_folder_and_leaves_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("data.zip");
    zip_of(
        &path,
        &[
            (
                "data/bili/json/search_contents_2026-11-01.json",
                &fixture(POSTS),
            ),
            (
                "data/bili/json/search_comments_2026-11-01.json",
                &fixture(COMMENTS),
            ),
            ("data/bili/csv/search_contents_2026-11-01.csv", CSV),
            ("data/bili/sqlite/data.db", SQLITE),
            (
                "data/bili/json/creator_creators_2026-11-01.json",
                b"[{\"creator_hash\": \"ab\"}]",
            ),
        ],
    );
    assert_posts(&listed(&path));
    // 两个日期的文件合起来：同一篇只列一次
    let two = tmp.path().join("two.zip");
    zip_of(
        &two,
        &[
            (
                "data/bili/json/search_contents_2026-11-01.json",
                &fixture(POSTS),
            ),
            (
                "data/bili/jsonl/detail_contents_2026-11-02.jsonl",
                &fixture(POSTS_JSONL),
            ),
        ],
    );
    assert_posts(&listed(&two));
}

#[test]
fn pages_are_cut_by_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let path = file_in(tmp.path(), POSTS, &fixture(POSTS));
    let out = plugin(
        &[
            "list",
            "file:x",
            "--method",
            "mediacrawler",
            "--cursor",
            "2",
        ],
        Some(&path),
    );
    let value = envelope(&out);
    assert_eq!(value["items"].as_array().unwrap().len(), 1);
    assert_eq!(value["items"][0]["url"], EXPECTED[2].0);
    assert_eq!(value["nextCursor"], Value::Null);
    let bad = plugin(
        &[
            "list",
            "file:x",
            "--method",
            "mediacrawler",
            "--cursor",
            "x",
        ],
        Some(&path),
    );
    assert_eq!(envelope(&bad)["error"]["code"], "invalid-args");
}

#[test]
fn list_refuses_what_is_not_a_file_import() {
    let tmp = tempfile::tempdir().unwrap();
    let path = file_in(tmp.path(), POSTS, &fixture(POSTS));
    let not_file = plugin(
        &["list", "bilibili:favorites", "--method", "mediacrawler"],
        Some(&path),
    );
    assert_eq!(envelope(&not_file)["error"]["code"], "invalid-args");
    let no_input = plugin(&["list", "file:x", "--method", "mediacrawler"], None);
    assert_eq!(envelope(&no_input)["error"]["code"], "invalid-args");
    // 这种方法没有 fetch
    let fetch = plugin(
        &["fetch", "m/0", "--out", "/o", "--method", "mediacrawler"],
        Some(&path),
    );
    assert_eq!(envelope(&fetch)["error"]["code"], "invalid-args");
}

#[test]
fn a_comments_file_is_refused_with_a_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let json = file_in(tmp.path(), COMMENTS, &fixture(COMMENTS));
    let jsonl = file_in(tmp.path(), COMMENTS_JSONL, &fixture(COMMENTS_JSONL));
    let only_comments = tmp.path().join("comments.zip");
    zip_of(
        &only_comments,
        &[(
            "data/bili/json/search_comments_2026-11-01.json",
            &fixture(COMMENTS),
        )],
    );
    for input in [json, jsonl, only_comments] {
        let error = failure(&input);
        assert_eq!(error["code"], "unsupported-input", "{error}");
        let hint = error["hint"].as_str().unwrap();
        assert!(hint.contains("评论") && hint.contains("contents"), "{hint}");
        assert!(
            error["detail"].as_str().unwrap().contains("comment_id"),
            "{error}"
        );
    }
}

#[test]
fn csv_sqlite_excel_and_other_files_are_refused_with_a_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let xlsx = tmp.path().join("x.xlsx");
    zip_of(
        &xlsx,
        &[
            ("xl/workbook.xml", b"<workbook/>"),
            ("[Content_Types].xml", b"<Types/>"),
        ],
    );
    let other_shape = file_in(
        tmp.path(),
        "o.json",
        b"[{\"creator_hash\": \"ab\", \"fans\": 3}]",
    );
    let empty = file_in(tmp.path(), "e.json", b"[]");
    for (name, bytes) in [("a.csv", CSV), ("a.db", SQLITE), ("a.txt", b"plain words")] {
        let path = file_in(tmp.path(), name, bytes);
        let error = failure(&path);
        assert_eq!(error["code"], "unsupported-input", "{name}: {error}");
        let hint = error["hint"].as_str().unwrap();
        assert!(hint.contains("CSV") && hint.contains("json"), "{hint}");
        assert!(error["detail"].as_str().unwrap().contains("file: input"));
    }
    for path in [xlsx, other_shape, empty] {
        let error = failure(&path);
        assert_eq!(error["code"], "unsupported-input", "{error}");
        assert!(error["hint"].as_str().unwrap().contains("CSV"));
    }
}

#[test]
fn malformed_files_are_answered_with_the_file_and_the_field() {
    let tmp = tempfile::tempdir().unwrap();
    let whole = fixture(POSTS);
    let cut = file_in(tmp.path(), "cut.json", &whole[..whole.len() - 30]);
    let error = failure(&cut);
    assert_eq!(error["code"], "unsupported-input");
    let detail = error["detail"].as_str().unwrap();
    assert!(
        detail.contains("file: input") && detail.contains("field: (top level)"),
        "{detail}"
    );

    let lines = String::from_utf8(fixture(POSTS_JSONL)).unwrap();
    let first = lines.lines().next().unwrap();
    let bad_line = file_in(
        tmp.path(),
        "l.jsonl",
        format!("{first}\n{{\"video_id\": \n").as_bytes(),
    );
    let detail = failure(&bad_line)["detail"].as_str().unwrap().to_owned();
    assert!(
        detail.contains("file: input") && detail.contains("field: (line 2)"),
        "{detail}"
    );

    // 看起来是帖子文件，但每条的 id 都不成样子：平台改了字段的写法
    let changed = file_in(tmp.path(), "changed.json", &fixture("changed.json"));
    let error = failure(&changed);
    assert_eq!(error["code"], "upstream-changed", "{error}");
    let detail = error["detail"].as_str().unwrap();
    assert!(
        detail.contains("file: input") && detail.contains("field: video_id"),
        "{detail}"
    );

    // zip 里的条目坏了：写明是哪个条目
    let zip = tmp.path().join("bad.zip");
    zip_of(
        &zip,
        &[("data/bili/json/bad_contents.json", b"[{\"video_id\": ")],
    );
    let error = failure(&zip);
    assert_eq!(error["code"], "unsupported-input");
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .contains("file: data/bili/json/bad_contents.json")
    );
    let not_zip = file_in(tmp.path(), "n.zip", b"PK\x03\x04broken");
    assert_eq!(failure(&not_zip)["code"], "unsupported-input");
}

#[test]
fn a_zip_with_hostile_entry_names_reads_none_of_them() {
    let tmp = tempfile::tempdir().unwrap();
    let evil = fixture(POSTS);
    let path = tmp.path().join("hostile.zip");
    zip_of(
        &path,
        &[
            ("../evil_contents.json", &evil),
            ("/abs/evil_contents.json", &evil),
            ("data/../../evil_contents.json", &evil),
            ("data\\evil_contents.json", &evil),
            ("C:/evil_contents.json", &evil),
            ("data/bili/json/ok\u{1}_contents.json", &evil),
        ],
    );
    // 只剩这些：当作没有这个文件
    let error = failure(&path);
    assert_eq!(error["code"], "unsupported-input", "{error}");
    // 和一个正常的条目放在一起：只列正常的那个
    let mixed = tmp.path().join("mixed.zip");
    let one = fixture(POSTS_JSONL);
    let first_line = String::from_utf8(one).unwrap();
    let first_line = first_line.lines().next().unwrap();
    zip_of(
        &mixed,
        &[
            ("../evil_contents.json", &evil),
            (
                "data/bili/jsonl/ok_contents_2026-11-01.jsonl",
                first_line.as_bytes(),
            ),
            ("data\\evil_contents.json", &evil),
        ],
    );
    let value = listed(&mixed);
    let items = value["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{value}");
    // 用 `..` 想写出去的位置什么也没有
    assert!(!tmp.path().join("evil_contents.json").exists());
    assert!(
        !tmp.path()
            .parent()
            .unwrap()
            .join("evil_contents.json")
            .exists()
    );
}

fn small() -> Limits {
    Limits {
        zip_entries: 3,
        file_bytes: 300,
        total_bytes: 500,
        records: 2,
    }
}

fn refused(path: &Path, limits: &Limits) -> (ErrorCode, String) {
    let failure = list("file:x", Some(path), None, limits)
        .unwrap_err()
        .into_failure(true);
    (failure.code, (*failure.detail.unwrap()).clone())
}

#[test]
fn files_over_the_limits_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    // 单个文件太大
    let big = file_in(tmp.path(), "big.json", &fixture(POSTS));
    let (code, detail) = refused(&big, &small());
    assert_eq!(code, ErrorCode::UnsupportedInput);
    assert!(detail.contains("byte limit"), "{detail}");
    // 记录太多：json 和 jsonl
    let roomy = Limits {
        file_bytes: 1 << 20,
        ..small()
    };
    for (name, bytes) in [(POSTS, fixture(POSTS)), (POSTS_JSONL, fixture(POSTS_JSONL))] {
        let path = file_in(tmp.path(), name, &bytes);
        let (code, detail) = refused(&path, &roomy);
        assert_eq!(code, ErrorCode::UnsupportedInput);
        assert!(detail.contains("records"), "{name}: {detail}");
    }
    // zip：条目太多；条目解压后太大；几个条目加起来太大
    let many = tmp.path().join("many.zip");
    zip_of(
        &many,
        &[
            ("a.json", b"[]"),
            ("b.json", b"[]"),
            ("c.json", b"[]"),
            ("d.json", b"[]"),
        ],
    );
    assert!(refused(&many, &roomy).1.contains("entries"));
    let one = tmp.path().join("one.zip");
    zip_of(&one, &[("a_contents.json", &fixture(POSTS))]);
    assert!(refused(&one, &small()).1.contains("byte limit"));
    let sum = tmp.path().join("sum.zip");
    let filler = format!("[{}]", " ".repeat(280));
    zip_of(
        &sum,
        &[("a.json", filler.as_bytes()), ("b.json", filler.as_bytes())],
    );
    let (code, detail) = refused(&sum, &small());
    assert_eq!(code, ErrorCode::UnsupportedInput);
    assert!(detail.contains("together"), "{detail}");
}

// ---- 清单经宿主装上，列经宿主调一次：输出要过宿主的 schema 检查。
// 文件按宿主的方式交来：副本固定叫 `input.<扩展名>`，路径放在 `USUYO_INPUT`，来源是 `file:<显示名>`。

fn host() -> (tempfile::TempDir, PluginHost) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("plugins/usuyo-fetch/bilibili/0.1.0");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("usuyo-plugin.yaml");
    std::fs::copy(manifest, dir.join("usuyo-plugin.yaml")).unwrap();
    std::fs::copy(BIN, dir.join("usuyo-fetch-bilibili")).unwrap();
    let found = discover(&tmp.path().join("plugins")).unwrap();
    assert!(found.rejected.is_empty(), "{:?}", found.rejected);
    let host = PluginHost::new(
        HostConfig {
            work_dir: tmp.path().join("work"),
            cache_dir: tmp.path().join("cache"),
            locale: "zh-CN".into(),
            limits: CallLimits::default(),
        },
        found.fetch,
    );
    (tmp, host)
}

#[tokio::test]
async fn the_manifest_loads_and_a_file_is_listed_through_the_host() {
    let (tmp, host) = host();
    let plugin = &host.plugins()[0];
    assert_eq!(plugin.id().as_str(), "usuyo-fetch/bilibili");
    let method = plugin.spec.method("mediacrawler").unwrap();
    let list = method.capabilities.list.as_ref().unwrap();
    assert!(!list.files.is_empty() && list.reads.is_some());
    let json = tmp.path().join("input.json");
    std::fs::write(&json, fixture(POSTS)).unwrap();
    let zip = tmp.path().join("input.zip");
    zip_of(
        &zip,
        &[("data/bili/jsonl/a_contents.jsonl", &fixture(POSTS_JSONL))],
    );
    for input in [json, zip] {
        let name = input.file_name().unwrap().to_str().unwrap().to_owned();
        let page = host
            .list(
                plugin,
                ListRequest {
                    method: "mediacrawler".into(),
                    source: format!("file:{name}"),
                    cursor: None,
                    since: None,
                    input_file: Some(input),
                },
                &CallOptions::default(),
            )
            .await
            .unwrap();
        let urls: Vec<&str> = page
            .items
            .iter()
            .map(|i| i.url.as_deref().unwrap())
            .collect();
        assert_eq!(urls, EXPECTED.map(|e| e.0), "{name}");
        // 只有链接，宿主交给平台自己的方法去取
        assert!(
            page.items
                .iter()
                .all(|i| i.reference.is_none() && i.acted_at.is_none())
        );
    }
    let health = host.health(plugin, &CallOptions::default()).await.unwrap();
    assert!(health.ready);
}
