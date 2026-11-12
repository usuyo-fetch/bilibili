//! `sources` 和 `list`：我自己的收藏夹和稍后再看（方法 `favorites`，mid 取自 cookie `DedeUserID`），
//! 和别人公开的收藏夹（方法 `favorites-public`，从你给的收藏夹或个人空间的链接开始，不带 cookie）。
//! 两种用同两个接口，都不调需要签名的接口。

use serde::Deserialize;
use usuyo_plugin_kit::protocol::{
    Access, Author, CallFailure, ErrorCode, Evidence, Identity, ListAction, ListItem, ListPage,
    PlatformId, Rfc3339, SourceInfo, SourcesOutput,
};

use crate::api::{Bilibili, SITE};
use crate::ids::{VideoRef, is_bvid};

const FAVORITE_PREFIX: &str = "bilibili:fav:";
/// 公开收藏夹的来源：`bilibili:fav-public:<收藏夹 id>`。
const PUBLIC_PREFIX: &str = "bilibili:fav-public:";
const WATCH_LATER: &str = "bilibili:toview";
/// 收藏夹每页条数（接口上限）。
const PAGE_SIZE: u32 = 20;
const MAX_PAGE: u32 = 10_000;
/// 收藏夹里 `type` 为 2 的是视频。
const TYPE_VIDEO: i64 = 2;

#[derive(Deserialize)]
struct Folders {
    list: Option<Vec<Folder>>,
}

#[derive(Deserialize)]
struct Folder {
    id: u64,
    title: String,
    #[serde(default)]
    media_count: u64,
}

#[derive(Deserialize)]
struct FavoritePage {
    medias: Option<Vec<FavoriteMedia>>,
    has_more: bool,
    #[serde(default)]
    info: Option<FolderInfo>,
}

#[derive(Deserialize)]
struct FolderInfo {
    #[serde(default)]
    title: String,
    #[serde(default)]
    media_count: Option<u64>,
}

#[derive(Deserialize)]
struct FavoriteMedia {
    #[serde(rename = "type")]
    kind: i64,
    #[serde(default)]
    bvid: String,
    title: String,
    upper: Upper,
    #[serde(default)]
    fav_time: i64,
    #[serde(default)]
    pubtime: i64,
    #[serde(default)]
    attr: i64,
}

#[derive(Deserialize)]
struct WatchLaterList {
    list: Option<Vec<WatchLaterEntry>>,
}

#[derive(Deserialize)]
struct WatchLaterEntry {
    #[serde(default)]
    bvid: String,
    title: String,
    owner: Upper,
    #[serde(default)]
    add_at: i64,
    #[serde(default)]
    pubdate: i64,
}

#[derive(Deserialize)]
struct Upper {
    mid: u64,
    name: String,
}

fn login_required(message: &str) -> CallFailure {
    CallFailure {
        hint: Some("请先在设置里登录 B站".into()),
        site: Some(SITE.into()),
        ..CallFailure::new(ErrorCode::LoginRequired, message)
    }
}

fn require_login(api: &Bilibili) -> Result<&str, CallFailure> {
    let session = api
        .session()
        .ok_or_else(|| login_required("no credentials were passed"))?;
    session
        .user_id()
        .ok_or_else(|| login_required("the credentials have no DedeUserID"))
}

fn at(seconds: i64) -> Option<Rfc3339> {
    (seconds > 0)
        .then(|| jiff::Timestamp::from_second(seconds).ok())
        .flatten()
        .map(Rfc3339::from_timestamp)
}

pub fn sources(api: &Bilibili) -> Result<SourcesOutput, CallFailure> {
    let mid = require_login(api)?;
    let reply = api.get("x/v3/fav/folder/created/list-all", &[("up_mid", mid)])?;
    let folders: Folders = reply.data()?;
    let mut sources: Vec<SourceInfo> = folders
        .list
        .unwrap_or_default()
        .into_iter()
        .map(|f| SourceInfo {
            source: format!("{FAVORITE_PREFIX}{}", f.id),
            title: f.title,
            action: Some(ListAction::Favorite),
            count: Some(f.media_count),
        })
        .collect();
    sources.push(SourceInfo {
        source: WATCH_LATER.into(),
        title: "稍后再看".into(),
        action: Some(ListAction::Favorite),
        count: None,
    });
    Ok(SourcesOutput { sources })
}

/// 一条链接指着谁的收藏夹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicLink {
    /// 一个收藏夹的 id（`media_id`）。
    Folder(String),
    /// 一个人的 mid：列他公开的收藏夹。
    User(String),
}

fn digits(s: &str) -> Option<String> {
    (!s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.to_owned())
}

/// 认链接：个人空间 `space.bilibili.com/<mid>[/favlist][?fid=<id>]`，收藏夹页
/// `www.bilibili.com/medialist/detail/ml<id>`、`…/list/ml<id>`，或带 `media_id=<id>` 的。不发请求。
pub fn public_link(link: &str) -> Option<PublicLink> {
    let url = url::Url::parse(link.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| digits(&v))
    };
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match host.as_str() {
        "space.bilibili.com" => {
            if let Some(id) = query("fid").or_else(|| query("media_id")) {
                return Some(PublicLink::Folder(id));
            }
            segments
                .first()
                .and_then(|m| digits(m))
                .map(PublicLink::User)
        }
        "www.bilibili.com" | "m.bilibili.com" => {
            if let Some(id) = query("media_id") {
                return Some(PublicLink::Folder(id));
            }
            segments
                .iter()
                .find_map(|s| s.strip_prefix("ml").and_then(digits))
                .map(PublicLink::Folder)
        }
        _ => None,
    }
}

/// `favorites-public` 的 `sources --link`：一个收藏夹就是它自己，一个人就是他公开的收藏夹。不带 cookie。
pub fn public_sources(api: &Bilibili, link: &str) -> Result<SourcesOutput, CallFailure> {
    let target = public_link(link).ok_or_else(|| CallFailure {
        hint: Some("这条链接不是 B站的收藏夹或个人空间".into()),
        ..CallFailure::new(
            ErrorCode::UnsupportedInput,
            "not a favourites folder or space link",
        )
    })?;
    let sources = match target {
        PublicLink::Folder(id) => {
            let reply = api.get(
                "x/v3/fav/resource/list",
                &[
                    ("media_id", id.as_str()),
                    ("pn", "1"),
                    ("ps", "20"),
                    ("platform", "web"),
                ],
            )?;
            let page: FavoritePage = reply.data()?;
            let info = page.info;
            vec![SourceInfo {
                source: format!("{PUBLIC_PREFIX}{id}"),
                title: info
                    .as_ref()
                    .map(|i| i.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| format!("收藏夹 {id}")),
                action: Some(ListAction::Favorite),
                count: info.and_then(|i| i.media_count),
            }]
        }
        PublicLink::User(mid) => {
            let reply = api.get(
                "x/v3/fav/folder/created/list-all",
                &[("up_mid", mid.as_str())],
            )?;
            let folders: Folders = reply.data()?;
            folders
                .list
                .unwrap_or_default()
                .into_iter()
                .map(|f| SourceInfo {
                    source: format!("{PUBLIC_PREFIX}{}", f.id),
                    title: f.title,
                    action: Some(ListAction::Favorite),
                    count: Some(f.media_count),
                })
                .collect()
        }
    };
    Ok(SourcesOutput { sources })
}

/// `favorites-public` 的 `list`：翻一个公开的收藏夹，不带 cookie。清单写 `unordered`：
/// 2026-11-12 实测一页里按收藏时间从旧到新，所以不拿 `--since` 提前停。
pub fn public_list(
    api: &Bilibili,
    source: &str,
    cursor: Option<&str>,
) -> Result<ListPage, CallFailure> {
    let id = source
        .strip_prefix(PUBLIC_PREFIX)
        .and_then(digits)
        .ok_or_else(|| {
            CallFailure::new(
                ErrorCode::UnsupportedInput,
                format!("unknown source `{source}`"),
            )
        })?;
    let (items, next_cursor) = favorites(api, &id, page_number(cursor)?, None)?;
    Ok(ListPage {
        items,
        next_cursor,
        evidence: Evidence {
            fetched_at: Rfc3339::from_timestamp(jiff::Timestamp::now()),
            access: Access::PublicWeb,
            site: None,
            upstream: None,
            status: None,
        },
    })
}

/// 列一页。`cursor` 是收藏夹的页码；稍后再看一次给完，没有游标。
pub fn list(
    api: &Bilibili,
    source: &str,
    cursor: Option<&str>,
    since: Option<&Rfc3339>,
) -> Result<ListPage, CallFailure> {
    require_login(api)?;
    let evidence = Evidence {
        fetched_at: Rfc3339::from_timestamp(jiff::Timestamp::now()),
        access: Access::SessionApi,
        site: Some(SITE.into()),
        upstream: None,
        status: None,
    };
    let (items, next_cursor) = if source == WATCH_LATER {
        if cursor.is_some() {
            return Err(CallFailure::new(
                ErrorCode::InvalidArgs,
                "the watch-later list has no cursor",
            ));
        }
        (watch_later(api, since)?, None)
    } else if let Some(id) = source.strip_prefix(FAVORITE_PREFIX) {
        if id.is_empty() || id.len() > 20 || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(CallFailure::new(
                ErrorCode::UnsupportedInput,
                format!("unknown source `{source}`"),
            ));
        }
        favorites(api, id, page_number(cursor)?, since)?
    } else {
        return Err(CallFailure::new(
            ErrorCode::UnsupportedInput,
            format!("unknown source `{source}`"),
        ));
    };
    Ok(ListPage {
        items,
        next_cursor,
        evidence,
    })
}

fn page_number(cursor: Option<&str>) -> Result<u32, CallFailure> {
    let Some(text) = cursor else { return Ok(1) };
    text.parse::<u32>()
        .ok()
        .filter(|n| (1..=MAX_PAGE).contains(n))
        .ok_or_else(|| {
            CallFailure::new(
                ErrorCode::InvalidArgs,
                "the cursor is not one this plugin gave",
            )
        })
}

fn favorites(
    api: &Bilibili,
    folder: &str,
    page: u32,
    since: Option<&Rfc3339>,
) -> Result<(Vec<ListItem>, Option<String>), CallFailure> {
    let pn = page.to_string();
    let ps = PAGE_SIZE.to_string();
    // order=mtime：按收藏时间从新到旧
    let reply = api.get(
        "x/v3/fav/resource/list",
        &[
            ("media_id", folder),
            ("pn", pn.as_str()),
            ("ps", ps.as_str()),
            ("order", "mtime"),
            ("type", "0"),
            ("tid", "0"),
            ("platform", "web"),
        ],
    )?;
    let data: FavoritePage = reply.data()?;
    let medias = data.medias.unwrap_or_default();
    let page_was_empty = medias.is_empty();
    let mut items = Vec::new();
    let mut skipped = 0usize;
    let mut reached_since = false;
    for media in medias {
        let acted_at = at(media.fav_time);
        if since
            .zip(acted_at.as_ref())
            .is_some_and(|(s, a)| a.timestamp() < s.timestamp())
        {
            reached_since = true;
            break;
        }
        // attr 0 和 4 是正常的；别的是失效视频（已删除、不可见）
        let usable =
            media.kind == TYPE_VIDEO && matches!(media.attr, 0 | 4) && is_bvid(&media.bvid);
        if !usable {
            skipped += 1;
            continue;
        }
        items.push(item(
            &media.bvid,
            media.title,
            &media.upper,
            at(media.pubtime),
            acted_at,
        )?);
    }
    if skipped > 0 {
        eprintln!(
            "skipped {skipped} entries that are not available videos (deleted, hidden or not a video)"
        );
    }
    // 翻到 MAX_PAGE 就停：再往后的游标 page_number 不认
    let next = (data.has_more && !page_was_empty && !reached_since && page < MAX_PAGE)
        .then(|| (page + 1).to_string());
    Ok((items, next))
}

fn watch_later(api: &Bilibili, since: Option<&Rfc3339>) -> Result<Vec<ListItem>, CallFailure> {
    let reply = api.get("x/v2/history/toview/web", &[])?;
    let data: WatchLaterList = reply.data()?;
    let mut entries = data.list.unwrap_or_default();
    // 接口的顺序没有保证；清单声明了 newest-first，这里自己按加入时间排
    entries.sort_by_key(|e| std::cmp::Reverse(e.add_at));
    let mut items = Vec::new();
    let mut skipped = 0usize;
    for entry in entries {
        let acted_at = at(entry.add_at);
        if since
            .zip(acted_at.as_ref())
            .is_some_and(|(s, a)| a.timestamp() < s.timestamp())
        {
            break;
        }
        if !is_bvid(&entry.bvid) {
            skipped += 1;
            continue;
        }
        items.push(item(
            &entry.bvid,
            entry.title,
            &entry.owner,
            at(entry.pubdate),
            acted_at,
        )?);
    }
    if skipped > 0 {
        eprintln!("skipped {skipped} watch-later entries without a valid BV id");
    }
    Ok(items)
}

fn item(
    bvid: &str,
    title: String,
    upper: &Upper,
    published_at: Option<Rfc3339>,
    acted_at: Option<Rfc3339>,
) -> Result<ListItem, CallFailure> {
    let video = VideoRef {
        bvid: bvid.to_owned(),
        page: 1,
    };
    let platform =
        PlatformId::try_from("bilibili").map_err(|e| CallFailure::new(ErrorCode::Internal, e))?;
    Ok(ListItem {
        url: Some(video.canonical_url()),
        reference: None,
        identity: Some(Identity::Platform {
            platform,
            content_id: video.content_id(),
            url: Some(video.canonical_url()),
        }),
        title: Some(title),
        author: Some(Author {
            name: upper.name.clone(),
            platform_id: Some(upper.mid.to_string()),
            url: Some(format!("https://space.bilibili.com/{}", upper.mid)),
        }),
        published_at,
        action: Some(ListAction::Favorite),
        acted_at,
    })
}
