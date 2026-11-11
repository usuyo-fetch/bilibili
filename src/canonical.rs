//! `canonicalize`：认出 B站视频的身份。`fetch` 用同一个函数，所以两边给的身份一样。

use usuyo_plugin_kit::protocol::{CallFailure, Canonicalized, ErrorCode, Identity, PlatformId};

use crate::api::Bilibili;
use crate::ids::{Parsed, VideoRef, parse_input};

/// 输入链接指向的视频；输入是短链时还带着展开后的完整链接。
pub struct Located {
    pub video: VideoRef,
    pub resolved_url: Option<String>,
}

/// 认出输入是哪个视频。`None` 是「不是 B站的视频链接」，不是错误。
/// 只有短链会发请求，请求不带 cookie，只跟随重定向。
pub fn locate(api: &Bilibili, input: &str) -> Result<Option<Located>, CallFailure> {
    let endpoints = api.endpoints();
    match parse_input(input, |h| endpoints.is_short_link_host(h)) {
        Parsed::Video(video) => Ok(Some(Located {
            video,
            resolved_url: None,
        })),
        Parsed::Foreign => Ok(None),
        Parsed::ShortLink(url) => {
            let expanded = api.expand_short_link(url)?;
            Ok(
                match parse_input(expanded.as_str(), |h| endpoints.is_short_link_host(h)) {
                    Parsed::Video(video) => Some(Located {
                        video,
                        resolved_url: Some(expanded.into()),
                    }),
                    Parsed::ShortLink(_) | Parsed::Foreign => None,
                },
            )
        }
    }
}

pub fn identity(video: &VideoRef) -> Result<Identity, CallFailure> {
    let platform =
        PlatformId::try_from("bilibili").map_err(|e| CallFailure::new(ErrorCode::Internal, e))?;
    Ok(Identity::Platform {
        platform,
        content_id: video.content_id(),
        url: Some(video.canonical_url()),
    })
}

pub fn canonicalize(api: &Bilibili, input: &str) -> Result<Canonicalized, CallFailure> {
    Ok(match locate(api, input)? {
        Some(found) => Canonicalized::Match {
            identity: identity(&found.video)?,
            resolved_url: found.resolved_url,
        },
        None => Canonicalized::NoMatch,
    })
}
