//! 链接和 id：BV 号、av 号、分 P 参数。纯函数，不发请求。

use url::Url;

/// BV 号用的 58 个字符；av 号和 BV 号互转用同一张表。
const ALPHABET: &[u8; 58] = b"FcwAPNKTMug3GV5Lj7EJnHpWsx4tb8haYeviqBz6rkCy12mUSDQX9RdoZf";
const XOR_CODE: u64 = 23_442_827_791_579;
const MAX_AID: u64 = 1 << 51;
const BVID_LEN: usize = 12;

/// 一个视频，加上要取的分 P（从 1 数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRef {
    pub bvid: String,
    pub page: u32,
}

impl VideoRef {
    /// `BV…`，分 P 大于 1 时写成 `BV…:p2`（协议 §2.3 里 bilibili 的 `contentId` 格式）。
    pub fn content_id(&self) -> String {
        if self.page > 1 {
            format!("{}:p{}", self.bvid, self.page)
        } else {
            self.bvid.clone()
        }
    }

    /// 规范链接，只用于去重和显示。
    pub fn canonical_url(&self) -> String {
        if self.page > 1 {
            format!(
                "https://www.bilibili.com/video/{}?p={}",
                self.bvid, self.page
            )
        } else {
            format!("https://www.bilibili.com/video/{}", self.bvid)
        }
    }
}

/// 输入链接是什么。
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Video(VideoRef),
    /// `b23.tv` 一类的短链，要展开才知道是什么。
    ShortLink(Url),
    /// 不是视频页（番剧、直播、空间页……）或根本不是 B站的链接。
    Foreign,
}

pub fn is_video_host(host: &str) -> bool {
    matches!(host, "www.bilibili.com" | "m.bilibili.com" | "bilibili.com")
}

/// 按链接形状分类。`is_short_host` 判断这个主机是不是短链主机。
pub fn parse_input(input: &str, is_short_host: impl Fn(&str) -> bool) -> Parsed {
    let Ok(url) = Url::parse(input.trim()) else {
        return Parsed::Foreign;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Parsed::Foreign;
    }
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return Parsed::Foreign;
    };
    let mut segments = url.path_segments().into_iter().flatten();
    let first = segments.next().unwrap_or_default();
    if is_video_host(&host) {
        if first != "video" {
            return Parsed::Foreign;
        }
        return match video_id(segments.next().unwrap_or_default()) {
            Some(bvid) => Parsed::Video(VideoRef {
                bvid,
                page: page_param(&url),
            }),
            None => Parsed::Foreign,
        };
    }
    if is_short_host(&host) {
        // `b23.tv/BV…` 这样直接带 id 的，不必展开
        return match video_id(first) {
            Some(bvid) => Parsed::Video(VideoRef {
                bvid,
                page: page_param(&url),
            }),
            None if !first.is_empty() => Parsed::ShortLink(url),
            None => Parsed::Foreign,
        };
    }
    Parsed::Foreign
}

/// `?p=2` 里的分 P；缺省、写错、为 0 都当第 1 个。
fn page_param(url: &Url) -> u32 {
    url.query_pairs()
        .find(|(k, _)| k == "p")
        .and_then(|(_, v)| v.parse::<u32>().ok())
        .filter(|p| (1..=10_000).contains(p))
        .unwrap_or(1)
}

/// 路径里的一段是 BV 号或 av 号时，给出 BV 号。
fn video_id(segment: &str) -> Option<String> {
    if segment.len() == BVID_LEN
        && segment
            .get(..2)
            .is_some_and(|p| p.eq_ignore_ascii_case("bv"))
    {
        let bvid = format!("BV{}", &segment[2..]);
        return is_bvid(&bvid).then_some(bvid);
    }
    let digits = segment.get(2..)?;
    if !segment.get(..2)?.eq_ignore_ascii_case("av") {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let aid: u64 = digits.parse().ok()?;
    (1..MAX_AID).contains(&aid).then(|| av_to_bv(aid))
}

pub fn is_bvid(text: &str) -> bool {
    text.len() == BVID_LEN
        && text.starts_with("BV1")
        && text.bytes().skip(2).all(|b| ALPHABET.contains(&b))
}

/// av 号换成 BV 号（公开的换算，不需要网络）。
pub fn av_to_bv(aid: u64) -> String {
    let mut bytes = *b"BV1000000000";
    let mut rest = (MAX_AID | aid) ^ XOR_CODE;
    for slot in bytes[3..].iter_mut().rev() {
        *slot = ALPHABET[(rest % 58) as usize];
        rest /= 58;
    }
    bytes.swap(3, 9);
    bytes.swap(4, 7);
    bytes.iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(host: &str) -> bool {
        host == "b23.tv"
    }

    fn content(input: &str) -> Option<String> {
        match parse_input(input, short) {
            Parsed::Video(v) => Some(v.content_id()),
            _ => None,
        }
    }

    #[test]
    fn av_numbers_convert_offline() {
        // 录下的 view 响应里 aid 170001 对应 BV17x411w7KC
        assert_eq!(av_to_bv(170001), "BV17x411w7KC");
        assert!(is_bvid("BV17x411w7KC"));
        assert!(!is_bvid("BV17x411w7K"));
        assert!(!is_bvid("BV17x411w7K0"));
    }

    #[test]
    fn canonicalize_table() {
        let cases = [
            (
                "https://www.bilibili.com/video/BV17x411w7KC",
                Some("BV17x411w7KC"),
            ),
            (
                "https://www.bilibili.com/video/BV17x411w7KC/",
                Some("BV17x411w7KC"),
            ),
            (
                "https://m.bilibili.com/video/BV17x411w7KC?share_source=copy_web",
                Some("BV17x411w7KC"),
            ),
            (
                "HTTPS://WWW.BILIBILI.COM/video/bV17x411w7KC",
                Some("BV17x411w7KC"),
            ),
            (
                "https://www.bilibili.com/video/av170001",
                Some("BV17x411w7KC"),
            ),
            (
                "https://www.bilibili.com/video/AV170001/",
                Some("BV17x411w7KC"),
            ),
            (
                "https://www.bilibili.com/video/BV17x411w7KC?p=2",
                Some("BV17x411w7KC:p2"),
            ),
            (
                "https://www.bilibili.com/video/BV17x411w7KC?spm_id_from=x&p=10&t=5",
                Some("BV17x411w7KC:p10"),
            ),
            (
                "https://www.bilibili.com/video/av170001?p=3",
                Some("BV17x411w7KC:p3"),
            ),
            // 第 1 个分 P 就是整条视频，写不写 `p=1` 是同一个身份
            (
                "https://www.bilibili.com/video/BV17x411w7KC?p=1",
                Some("BV17x411w7KC"),
            ),
            (
                "https://www.bilibili.com/video/BV17x411w7KC?p=0",
                Some("BV17x411w7KC"),
            ),
            (
                "https://www.bilibili.com/video/BV17x411w7KC?p=abc",
                Some("BV17x411w7KC"),
            ),
            ("https://b23.tv/BV17x411w7KC", Some("BV17x411w7KC")),
            ("https://b23.tv/av170001?p=2", Some("BV17x411w7KC:p2")),
            ("https://www.bilibili.com/video/BV17x411w7K", None),
            ("https://www.bilibili.com/video/av0", None),
            (
                "https://www.bilibili.com/video/av99999999999999999999",
                None,
            ),
            ("https://www.bilibili.com/bangumi/play/ep1", None),
            ("https://space.bilibili.com/1", None),
            ("https://example.com/video/BV17x411w7KC", None),
            ("ftp://www.bilibili.com/video/BV17x411w7KC", None),
            ("not a url", None),
        ];
        for (input, want) in cases {
            assert_eq!(content(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn short_links_need_expanding() {
        assert!(matches!(
            parse_input("https://b23.tv/AbC123x", short),
            Parsed::ShortLink(_)
        ));
        assert_eq!(parse_input("https://b23.tv/", short), Parsed::Foreign);
    }

    #[test]
    fn canonical_urls() {
        let v = VideoRef {
            bvid: "BV17x411w7KC".into(),
            page: 2,
        };
        assert_eq!(
            v.canonical_url(),
            "https://www.bilibili.com/video/BV17x411w7KC?p=2"
        );
        let v = VideoRef { page: 1, ..v };
        assert_eq!(
            v.canonical_url(),
            "https://www.bilibili.com/video/BV17x411w7KC"
        );
    }
}
