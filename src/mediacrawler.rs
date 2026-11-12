//! `mediacrawler` 方法（协议 §3.9）：读你自己跑 MediaCrawler 留在磁盘上的B站帖子文件，每篇列成一条链接，
//! 交给本插件自己的方法去取。不联网，只读你交来的那一个文件；不读评论，也不读 CSV、SQLite。
//!
//! 文件是 MediaCrawler 保存格式为 json 或 jsonl 时的「contents」文件，按里面记录的字段认，不看文件名；
//! 可以是单独一个文件，或把 data 文件夹压成的 zip（zip 里的条目只按名字读进内存）。
//! 记录里的昵称、时间照文件写的列出：时间是帖子的发布时间，不是你存下它的时间。

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use jiff::Timestamp;
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use usuyo_plugin_kit::protocol::{
    Access, Author, CallFailure, ErrorCode, Evidence, Identity, ListItem, ListPage, Rfc3339,
};
use zip::ZipArchive;

use crate::canonical;
use crate::ids::{Parsed, parse_input};

// ---- 这个平台的部分：记录里的字段名，和链接怎么建。

/// 平台名：中文、英文，写进 `hint`。
const LABEL: (&str, &str) = ("B站", "Bilibili");
/// 帖子记录里一定有的字段，第一个是帖子的 id。
const FIELDS: &[&str] = &["video_id", "video_play_count"];
/// 标题取哪个字段，前面的是空的就取后面的；取第一个非空行。
const TITLE: &[&str] = &["title", "desc"];
const AUTHOR: &str = "nickname";
const TIME: &str = "create_time";

/// 这个平台上记录都列：没有不取的种类。
fn unlisted(_: &Record) -> bool {
    false
}

/// 文件里的 `video_id` 是 av 号：走本插件认链接的代码换成 BV 号，身份和别的方法给的一样。
fn link(record: &Record) -> Result<Linked, Refusal> {
    let Some(aid) = text_of(record, "video_id") else {
        return Ok(Linked::Invalid);
    };
    let Parsed::Video(video) =
        parse_input(&format!("https://www.bilibili.com/video/av{aid}"), |_| {
            false
        })
    else {
        return Ok(Linked::Invalid);
    };
    let identity =
        canonical::identity(&video).map_err(|e| Refusal::Internal(e.message.to_string()))?;
    Ok(Linked::Post(identity, video.canonical_url()))
}

// ---- 读文件的部分：六个平台的插件里一样，只有上面「这个平台的部分」不同。

/// 一页最多多少条。
const PAGE: usize = 500;
/// 时间戳大于等于它按毫秒算，小于它按秒算（秒数到 5138 年才到这个数）。
const MILLIS_FROM: i64 = 100_000_000_000;
/// 标题最多多少个字符。
const TITLE_CHARS: usize = 100;

/// 交来的文件不可信：各处读多少、数多少都有上限。测试用小的上限，运行用 [`Limits::default`]。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// zip 里最多多少个条目。
    pub zip_entries: usize,
    /// 一个 JSON 文件最多多少字节（单独的文件看文件大小，zip 里看解压后读到的字节）。
    pub file_bytes: u64,
    /// zip 里读的 JSON 文件加起来最多多少字节。
    pub total_bytes: u64,
    /// 所有文件加起来最多多少条记录。
    pub records: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            zip_entries: 100_000,
            file_bytes: 256 << 20,
            total_bytes: 1 << 30,
            records: 1_000_000,
        }
    }
}

/// `hint` 的几句话。
#[derive(Debug, Clone, Copy)]
pub enum Hint {
    Comments,
    NotAResultFile,
    TooLarge,
    Changed,
}

impl Hint {
    fn text(self, zh: bool) -> String {
        let (name_zh, name_en) = LABEL;
        match (self, zh) {
            (Hint::Comments, true) => format!(
                "这是 MediaCrawler 存下的评论文件，这里不读评论：请选同一次运行存下的{name_zh}帖子文件（名字里带 contents，保存格式用 json 或 jsonl），或把 data 文件夹压成 zip"
            ),
            (Hint::Comments, false) => format!(
                "This is a MediaCrawler comments file and comments are not read: choose the {name_en} posts file from the same run (its name contains contents, saved as json or jsonl), or zip the data folder"
            ),
            (Hint::NotAResultFile, true) => format!(
                "读不出 MediaCrawler 存下的{name_zh}帖子文件：CSV、SQLite、Excel 格式读不了。请把 MediaCrawler 的保存格式设成 json 或 jsonl 重新运行，再选名字里带 contents 的那个文件，或把 data 文件夹压成 zip"
            ),
            (Hint::NotAResultFile, false) => format!(
                "This does not read as a MediaCrawler {name_en} posts file: CSV, SQLite and Excel are not read. Run MediaCrawler again with the save format set to json or jsonl, then choose the file whose name contains contents, or zip the data folder"
            ),
            (Hint::TooLarge, true) => {
                "这个文件超过了插件愿意读的大小：少采集一些，或按日期拆开再选".into()
            }
            (Hint::TooLarge, false) => {
                "This file is larger than the plugin will read: collect less, or split it by date and choose one part".into()
            }
            (Hint::Changed, true) => {
                "MediaCrawler 存下的帖子文件和这个插件认得的不一样，插件要更新".into()
            }
            (Hint::Changed, false) => {
                "The MediaCrawler posts file differs from what this plugin reads; the plugin needs an update".into()
            }
        }
    }
}

/// 读文件失败的几种方式；到入口才变成协议的错误（[`Refusal::into_failure`]）。
#[derive(Debug)]
pub enum Refusal {
    InvalidArgs(String),
    /// 不是这个方法读得了的文件。
    Unsupported {
        message: String,
        detail: String,
        hint: Hint,
    },
    /// 认得出是帖子文件，但字段的形状变了。
    Changed {
        message: String,
        detail: String,
    },
    Internal(String),
}

impl Refusal {
    /// `file` 是交来的文件里的哪一个（zip 里的条目名，或 `input`），`field` 是哪个字段。
    fn unsupported(file: &str, field: &str, message: impl Into<String>, hint: Hint) -> Self {
        let message = message.into();
        Refusal::Unsupported {
            detail: format!("file: {file}\nfield: {field}\n{message}"),
            message,
            hint,
        }
    }

    fn changed(file: &str, field: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Refusal::Changed {
            detail: format!("file: {file}\nfield: {field}\n{message}"),
            message,
        }
    }

    /// `zh`：`hint` 写中文还是英文。
    pub fn into_failure(self, zh: bool) -> CallFailure {
        match self {
            Refusal::InvalidArgs(m) => {
                CallFailure::new(ErrorCode::InvalidArgs, format!("invalid arguments: {m}"))
            }
            Refusal::Unsupported {
                message,
                detail,
                hint,
            } => CallFailure {
                hint: Some(hint.text(zh)),
                ..CallFailure::new(
                    ErrorCode::UnsupportedInput,
                    format!("unsupported input: {message}"),
                )
            }
            .with_detail(&detail),
            Refusal::Changed { message, detail } => CallFailure {
                hint: Some(Hint::Changed.text(zh)),
                ..CallFailure::new(
                    ErrorCode::UpstreamChanged,
                    format!("MediaCrawler file format changed: {message}"),
                )
            }
            .with_detail(&detail),
            Refusal::Internal(m) => CallFailure::new(ErrorCode::Internal, m),
        }
    }
}

/// 一条帖子记录认出了没有。
enum Linked {
    /// 身份和规范链接。
    Post(Identity, String),
    /// 记录里的 id 不成样子。
    Invalid,
}

type Record = Map<String, Value>;

fn text_of(record: &Record, key: &str) -> Option<String> {
    match record.get(key)? {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 秒或毫秒的 Unix 时间；0、空、写错的都当没有。
fn epoch(value: Option<&Value>) -> Option<Timestamp> {
    let n = match value? {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    if n <= 0 {
        return None;
    }
    if n >= MILLIS_FROM {
        Timestamp::from_millisecond(n).ok()
    } else {
        Timestamp::from_second(n).ok()
    }
}

fn title_of(record: &Record) -> Option<String> {
    TITLE.iter().find_map(|key| {
        let text = text_of(record, key)?;
        let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
        Some(line.chars().take(TITLE_CHARS).collect())
    })
}

/// 一个文件里的记录属于哪一种。
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// 这个平台的帖子文件。
    Posts,
    /// 评论文件。
    Comments,
    /// 别的：创作者、动态、别的平台的帖子……
    Other,
}

fn kind_of(first: &Record) -> Kind {
    if first.contains_key("comment_id") {
        Kind::Comments
    } else if FIELDS.iter().all(|key| first.contains_key(*key)) {
        Kind::Posts
    } else {
        Kind::Other
    }
}

/// 一个读进来的 JSON 文件：名字和里面的记录（还没解析）。
struct PostsFile {
    name: String,
    records: Vec<Box<RawValue>>,
}

/// 文件里的记录：一个 JSON 数组（保存格式 json），或一行一个对象（jsonl，MediaCrawler 的默认）。
fn split_records(bytes: &[u8], name: &str, limits: &Limits) -> Result<Vec<Box<RawValue>>, Refusal> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let first = bytes.iter().copied().find(|b| !b.is_ascii_whitespace());
    let records: Vec<Box<RawValue>> = match first {
        Some(b'[') => serde_json::from_slice(bytes).map_err(|e| {
            Refusal::unsupported(
                name,
                "(top level)",
                format!("not a valid JSON array: {e}"),
                Hint::NotAResultFile,
            )
        })?,
        Some(b'{') => {
            let mut out = Vec::new();
            for (n, line) in bytes.split(|b| *b == b'\n').enumerate() {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                if out.len() >= limits.records {
                    return Err(too_many(name, limits));
                }
                out.push(serde_json::from_slice(line).map_err(|e| {
                    Refusal::unsupported(
                        name,
                        &format!("(line {})", n + 1),
                        format!("not a valid JSON line: {e}"),
                        Hint::NotAResultFile,
                    )
                })?);
            }
            out
        }
        _ => {
            return Err(Refusal::unsupported(
                name,
                "(top level)",
                "the file is neither a JSON array nor JSON lines",
                Hint::NotAResultFile,
            ));
        }
    };
    if records.len() > limits.records {
        return Err(too_many(name, limits));
    }
    Ok(records)
}

fn too_many(file: &str, limits: &Limits) -> Refusal {
    Refusal::unsupported(
        file,
        "(records)",
        format!("more than {} records", limits.records),
        Hint::TooLarge,
    )
}

/// 把一个 JSON 文件按记录分类：帖子文件放进 `posts`，评论文件记一笔。
fn sort_file(
    bytes: &[u8],
    name: &str,
    limits: &Limits,
    posts: &mut Vec<PostsFile>,
    saw_comments: &mut bool,
) -> Result<(), Refusal> {
    let records = split_records(bytes, name, limits)?;
    let first = records
        .first()
        .and_then(|raw| serde_json::from_str::<Record>(raw.get()).ok());
    match first.as_ref().map(kind_of) {
        Some(Kind::Posts) => posts.push(PostsFile {
            name: name.to_owned(),
            records,
        }),
        Some(Kind::Comments) => *saw_comments = true,
        Some(Kind::Other) | None => {}
    }
    Ok(())
}

/// 名字能不能当 zip 里的一条正常条目：不会指到别处。
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
        && !name.split('/').any(|seg| seg == "..")
        && name.as_bytes().get(1) != Some(&b':')
}

fn is_json_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".json") || lower.ends_with(".jsonl")
}

/// 读 zip：只按名字读里面的 JSON 条目进内存，不解压到磁盘；名字不正常的条目当没有。
fn read_zip(
    file: File,
    limits: &Limits,
    posts: &mut Vec<PostsFile>,
    saw_comments: &mut bool,
) -> Result<(), Refusal> {
    let mut archive = ZipArchive::new(file).map_err(|e| {
        Refusal::unsupported(
            PLAIN,
            "(zip)",
            format!("not a readable zip file: {e}"),
            Hint::NotAResultFile,
        )
    })?;
    if archive.len() > limits.zip_entries {
        return Err(Refusal::unsupported(
            PLAIN,
            "(zip entries)",
            format!(
                "the zip has {} entries, over the {} limit",
                archive.len(),
                limits.zip_entries
            ),
            Hint::TooLarge,
        ));
    }
    // 名字读不出来（不是 UTF-8）的条目当没有；同名的取第一个；按名字排，日期在名字里，合并的先后固定
    let mut entries: BTreeMap<String, usize> = BTreeMap::new();
    for i in 0..archive.len() {
        if let Some(Ok(name)) = archive.name_for_index(i)
            && safe_name(&name)
            && is_json_name(&name)
        {
            entries.entry(name.into_owned()).or_insert(i);
        }
    }
    let mut total = 0u64;
    for (name, index) in entries {
        let entry = archive.by_index(index).map_err(|e| {
            Refusal::unsupported(
                &name,
                "(zip entry)",
                format!("cannot read the entry: {e}"),
                Hint::NotAResultFile,
            )
        })?;
        let over =
            |message: String| Refusal::unsupported(&name, "(zip entry)", message, Hint::TooLarge);
        if entry.size() > limits.file_bytes {
            return Err(over(format!(
                "the entry is {} bytes, over the {} byte limit",
                entry.size(),
                limits.file_bytes
            )));
        }
        let mut bytes = Vec::new();
        entry
            .take(limits.file_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| {
                Refusal::unsupported(
                    &name,
                    "(zip entry)",
                    format!("cannot read the entry: {e}"),
                    Hint::NotAResultFile,
                )
            })?;
        if bytes.len() as u64 > limits.file_bytes {
            return Err(over(format!(
                "the entry is over the {} byte limit when unpacked",
                limits.file_bytes
            )));
        }
        total += bytes.len() as u64;
        if total > limits.total_bytes {
            return Err(over(format!(
                "the JSON entries together are over the {} byte limit",
                limits.total_bytes
            )));
        }
        sort_file(&bytes, &name, limits, posts, saw_comments)?;
    }
    Ok(())
}

/// 交来的单个文件在报错里叫什么。
const PLAIN: &str = "input";

/// 打开交来的文件（zip，或单独一个 JSON 文件），拿出里面的帖子文件。
fn load(path: &Path, limits: &Limits) -> Result<Vec<PostsFile>, Refusal> {
    let mut file =
        File::open(path).map_err(|e| Refusal::Internal(format!("cannot open the input: {e}")))?;
    let mut magic = [0u8; 4];
    let n = file
        .read(&mut magic)
        .map_err(|e| Refusal::Internal(format!("cannot read the input: {e}")))?;
    let is_zip = n == 4 && magic.starts_with(b"PK") && matches!(&magic[2..], [3, 4] | [5, 6]);
    let mut posts = Vec::new();
    let mut saw_comments = false;
    if is_zip {
        read_zip(file, limits, &mut posts, &mut saw_comments)?;
    } else {
        let len = file
            .metadata()
            .map_err(|e| Refusal::Internal(format!("cannot read the input: {e}")))?
            .len();
        if len > limits.file_bytes {
            return Err(Refusal::unsupported(
                PLAIN,
                "(whole file)",
                format!(
                    "the file is {len} bytes, over the {} byte limit",
                    limits.file_bytes
                ),
                Hint::TooLarge,
            ));
        }
        let mut bytes = magic[..n].to_vec();
        file.read_to_end(&mut bytes)
            .map_err(|e| Refusal::Internal(format!("cannot read the input: {e}")))?;
        sort_file(&bytes, PLAIN, limits, &mut posts, &mut saw_comments)?;
    }
    if posts.is_empty() {
        let (field, message, hint) = if saw_comments {
            (
                "comment_id",
                "a comments file, not a posts file",
                Hint::Comments,
            )
        } else {
            (
                "(records)",
                "no posts file of this platform in the input",
                Hint::NotAResultFile,
            )
        };
        return Err(Refusal::unsupported(PLAIN, field, message, hint));
    }
    Ok(posts)
}

struct Entry {
    item: ListItem,
    published: Option<i64>,
}

/// 每条帖子一个链接条目：链接用插件自己的链接代码，所以身份和别的方法给的一样；同一个身份只留第一条
/// （MediaCrawler 的 JSON 一直往后追加，同一篇会存几遍）。新的在前，没有时间的在最后。
fn entries(files: &[PostsFile]) -> Result<Vec<ListItem>, Refusal> {
    let mut out: Vec<Entry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut unreadable = 0usize;
    let mut first_problem: Option<(String, String)> = None;
    let mut not_ours = 0usize;
    for file in files {
        for raw in &file.records {
            let Ok(record) = serde_json::from_str::<Record>(raw.get()) else {
                unreadable += 1;
                first_problem
                    .get_or_insert((file.name.clone(), "a record is not an object".into()));
                continue;
            };
            if !FIELDS.iter().all(|key| record.contains_key(*key)) {
                unreadable += 1;
                first_problem.get_or_insert((
                    file.name.clone(),
                    format!("a record lacks one of {FIELDS:?}"),
                ));
                continue;
            }
            if unlisted(&record) {
                not_ours += 1;
                continue;
            }
            match link(&record)? {
                Linked::Post(identity, url) => {
                    if !seen.insert(url.clone()) {
                        continue;
                    }
                    let published = epoch(record.get(TIME));
                    out.push(Entry {
                        item: ListItem {
                            url: Some(url),
                            reference: None,
                            identity: Some(identity),
                            title: title_of(&record),
                            author: text_of(&record, AUTHOR).map(|name| Author {
                                name,
                                platform_id: None,
                                url: None,
                            }),
                            published_at: published.map(Rfc3339::from_timestamp),
                            action: None,
                            acted_at: None,
                        },
                        published: published.map(|t| t.as_second()),
                    });
                }
                Linked::Invalid => {
                    unreadable += 1;
                    first_problem.get_or_insert((
                        file.name.clone(),
                        format!("the `{}` of a record is not a usable id", FIELDS[0]),
                    ));
                }
            }
        }
    }
    if unreadable > 0 {
        let (file, problem) = first_problem.unwrap_or_default();
        eprintln!("{unreadable} records were skipped: {file}: {problem}");
        if out.is_empty() && not_ours == 0 {
            return Err(Refusal::changed(
                &file,
                FIELDS[0],
                format!("none of the records is readable: {problem}"),
            ));
        }
    }
    if not_ours > 0 {
        eprintln!("{not_ours} records are not listed: this plugin does not fetch their kind");
    }
    out.sort_by_key(|e| Reverse(e.published));
    Ok(out.into_iter().map(|e| e.item).collect())
}

/// 列一页。`cursor` 是从第几条开始（插件自己的不透明字符串）。
pub fn list(
    source: &str,
    input: Option<&Path>,
    cursor: Option<&str>,
    limits: &Limits,
) -> Result<ListPage, Refusal> {
    if !source.starts_with("file:") {
        return Err(Refusal::InvalidArgs(format!(
            "source `{source}` is not a file import"
        )));
    }
    let input = input
        .ok_or_else(|| Refusal::InvalidArgs("USUYO_INPUT is not set for a file import".into()))?;
    let start = match cursor {
        None => 0,
        Some(c) => c
            .parse::<usize>()
            .map_err(|_| Refusal::InvalidArgs(format!("cursor `{c}` is not ours")))?,
    };
    let fetched_at = Rfc3339::from_timestamp(Timestamp::now());
    let all = entries(&load(input, limits)?)?;
    let end = start.saturating_add(PAGE).min(all.len());
    let items = all.get(start..end).unwrap_or_default().to_vec();
    Ok(ListPage {
        items,
        next_cursor: (end < all.len()).then(|| end.to_string()),
        evidence: Evidence {
            fetched_at,
            access: Access::PrivateApi,
            site: None,
            upstream: None,
            status: None,
        },
    })
}

/// 入口用的一次调用：交来的文件在 `USUYO_INPUT`，`hint` 的语言看 `USUYO_LOCALE`。
pub fn list_call(source: &str, cursor: Option<&str>) -> Result<ListPage, CallFailure> {
    let zh = std::env::var("USUYO_LOCALE")
        .ok()
        .is_none_or(|l| l.to_ascii_lowercase().starts_with("zh"));
    let input = std::env::var_os("USUYO_INPUT").map(std::path::PathBuf::from);
    list(source, input.as_deref(), cursor, &Limits::default())
        .map_err(|refusal| refusal.into_failure(zh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let at = |v: Value| epoch(Some(&v)).map(|t| t.to_string());
        assert_eq!(
            at(Value::from(1_790_856_000)).as_deref(),
            Some("2026-10-01T12:00:00Z")
        );
        assert_eq!(
            at(Value::from(1_790_856_000_000_i64)).as_deref(),
            Some("2026-10-01T12:00:00Z")
        );
        assert_eq!(
            at(Value::from("1790856000")).as_deref(),
            Some("2026-10-01T12:00:00Z")
        );
        assert_eq!(at(Value::from(0)), None);
        assert_eq!(at(Value::from(-5)), None);
        assert_eq!(at(Value::from("later")), None);
        assert_eq!(at(Value::Null), None);
        assert_eq!(epoch(None), None);
    }

    #[test]
    fn names() {
        assert!(safe_name("data/x/json/a_contents_2026-11-01.json"));
        assert!(!safe_name("../a.json"));
        assert!(!safe_name("a/../../b.json"));
        assert!(!safe_name("/etc/a.json"));
        assert!(!safe_name("a\\b.json"));
        assert!(!safe_name("C:/a.json"));
        assert!(!safe_name("a\0b.json"));
        assert!(is_json_name("A.JSONL") && is_json_name("b.json") && !is_json_name("c.csv"));
    }
}
