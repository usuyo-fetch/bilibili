# usuyo-fetch/bilibili

[usuyo](https://github.com/usuyo/usuyo) 的 B站获取插件。把你在 B站已经收下的内容（自己建的收藏夹、稍后再看）和单个视频，存到自己电脑上，用于个人备份和离线阅读。它是一个独立程序，按 usuyo 的插件协议（`docs/protocol/`）和宿主对话，不依赖宿主的代码。

## 做什么

| 子命令 | 说明 |
|---|---|
| `canonicalize` | 认出 BV 号、av 号、`b23.tv` 短链和分 P（`BV…:p2`）。av 号离线换成 BV 号；只有短链会发请求，只跟随重定向，不带 cookie |
| `sources` | 列出你自己的收藏夹和「稍后再看」。你的 mid 取自 cookie `DedeUserID` |
| `list` | 一页一调，收藏夹按页码翻页，`actedAt` 是收藏时间；稍后再看一次给完，按加入时间从新到旧 |
| `fetch` | 元数据（`view`）、封面、按需要的低画质音视频流。不用账号：即使你登录过，也不读凭据、不带 cookie |
| `health` | 不发请求；`--probe` 才用登录信息请求一次 `nav` |

`fetch` 的输出目录：

```
view.json                  view 接口的原始响应
meta.json                  分 P、文字来源、流的说明
images/cover.<ext>         封面
media/video.m4s            视频流（没有声音）
media/audio.m4s            音频流
```

视频流和音频流是两个文件，这里没有合并；合并交给宿主的转换步骤。两个都存了时，`media[]` 里这两项带同一个 `group`（`cid:<这一 P 的 cid>`），宿主据此配对。`--want` 里有 `media:video` 时音频也一起写；只要 `media:audio` 时只写音频。拿不到的部分记进 `missing` 和 `missingReason`，不会写一个空文件。

错误码：`-412`、`-352`（以及 HTTP 412、响应里带 `v_voucher`）回 `blocked`，`-101` 回 `login-required`，`-799`、`-509`、HTTP 429 回 `rate-limited`。

## 不做什么

- 点赞列表、评论、弹幕。
- 字幕：要登录才取得到，`fetch` 不用账号，所以不给；语音转文字由 usuyo 自己做。
- 高清画质：始终选能拿到的最低画质，不碰需要签名的接口。
- 番剧、直播、音频区、订阅的合集。
- 登录：登录窗口和 cookie 由宿主管，插件只读宿主交来的 `USUYO_CREDENTIALS`。插件不读浏览器的 cookie 库，不保存、不打印 cookie 的值，不调 cookie 刷新接口，不带 `bili_jct`。
- cookie 只用于 `sources` 和 `list`，只发往 `api.bilibili.com`；`fetch`、`canonicalize` 不带 cookie，封面和视频流的请求也不带，并且只连 `*.hdslb.com`、`*.bilivideo.com`、`*.bilivideo.cn`。

## 节奏

清单里的建议值：`batch: true`，两次调用至少隔 5 秒，每天不超过 500 次调用。B站对带登录态的批量下载有长期的风控；`fetch` 不带登录态，列清单才用账号。所以：

- 平时只取文字、元数据和封面（`--want text,meta,images`），视频流等你要看的时候再取。
- 第一次列清单不要一口气翻几百页；出现 `blocked` 时宿主会暂停这个插件，等一阵再继续。
- 同一个账号不要同时开别的下载工具。

## 构建和测试

```
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

测试对着本机的假服务器跑，响应样例在 `tests/fixtures/`，其中 `view.json` 是一次真实的 `view` 响应（公开视频，封面地址换成了本机地址），其余是照公开项目的代码手写的。

## 还没有用真实账号核对的

`x/v3/fav/folder/created/list-all`、`x/v3/fav/resource/list`、`x/v2/history/toview/web` 的字段名取自 bili-sync（MIT）的解析代码，接口路径和参数对照 yutto 的写法；`x/player/playurl` 不带签名、不带 cookie 调用是否一直可用，`health --probe` 用的 `nav` 的响应形状，都要用你的账号试。
