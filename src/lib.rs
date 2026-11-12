//! usuyo 的 B站获取插件：把你在 B站已经收下的内容（收藏夹、稍后再看）存到自己电脑上，
//! 用于个人备份和离线阅读。协议见 usuyo 仓库的 `docs/protocol/`。

pub mod api;
pub mod canonical;
pub mod favorites;
pub mod ids;
pub mod mediacrawler;
pub mod session;
pub mod video;
