# usuyo-fetch/bilibili

[usuyo](https://github.com/usuyo/usuyo) 的 B站获取插件。把你在 B站已经收下的内容（自己建的收藏夹、稍后再看）和单个视频，存到自己电脑上，用于个人备份和离线阅读。它是一个独立程序，按 usuyo 的插件协议（`docs/protocol/`）和宿主对话，不依赖宿主的代码。

## 做什么

几种取法各是清单里的一种方法（usuyo 协议 §2.3.1），调用时经 `--method` 指定：

| 方法 | 做什么 | 账号 |
|---|---|---|
| `view` | `fetch`：元数据（`view`）、封面、按需要的低画质音视频流 | 不用：即使你登录过，也不读凭据、不带 cookie |
| `favorites-public` | `sources --link`：你给一个人的个人空间（`space.bilibili.com/<mid>`）就列他公开的收藏夹，给一个收藏夹的链接就是它；`list`：翻一个公开收藏夹，一页 20 条，`actedAt` 是收藏时间；2026-11-12 实测一页里从旧到新，清单写 `unordered` | 不用 |
| `favorites` | `sources`：你自己的收藏夹和「稍后再看」，mid 取自 cookie `DedeUserID`；`list`：收藏夹按页码翻页，稍后再看一次给完，按加入时间从新到旧 | 在 usuyo 里登录的 B站 |
| `userscript` | 不在插件里跑：`userscript/` 里的脚本在你的浏览器里把收藏夹、稍后再看交给 usuyo | 只在你的浏览器里 |
| `yutto` | `fetch`：包你自己装的 yutto（GPL-3.0，独立进程，插件不带、不下载、不链接它），取高画质的画面、声音和字幕；`session-api`，默认开着，要等登录以后才用得上；已实现，只对着假的 yutto 测过，没有对着真的 yutto 和 B站 跑过；见下面「yutto」 | 在 usuyo 里登录的 B站 |
| `mediacrawler` | `list`：读你自己跑 MediaCrawler 留下的 B站视频文件（json 或 jsonl，或把 data 文件夹压成的 zip），每个视频列成一条链接（文件里的 av 号换成 BV 号），带标题、作者昵称、发布时间；不读评论、CSV、SQLite；`private-api`，默认关；已实现，只对着手写的文件测过，没有对着 MediaCrawler 真正的输出比对过 | 不用：文件是你自己用登录了账号的 MediaCrawler 跑出来的，这种方法不联网 |

| 子命令 | 说明 |
|---|---|
| `canonicalize` | 认出 BV 号、av 号、`b23.tv` 短链和分 P（`BV…:p2`）。av 号离线换成 BV 号；只有短链会发请求，只跟随重定向，不带 cookie。不分方法 |
| `health` | 不发请求，只看本机：yutto 和 ffmpeg 找没找到、版本够不够（没过只挡方法 `yutto`，别的方法照用）；`--probe` 才用登录信息请求一次 `nav` |

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

## yutto

方法 `yutto` 用的程序是你自己装的，插件不带、不下载、不安装它们，只按名字在 `PATH` 和 `/opt/homebrew/bin`、`/usr/local/bin` 里找：

- yutto 2.3.1 以上：`uv tool install yutto`（来源：[yutto](https://github.com/yutto-dev/yutto)，GPL-3.0，插件只把它当独立程序起一个进程）。
- ffmpeg：macOS 上 `brew install ffmpeg`。yutto 每次都用它把流封成 `mp4`、`m4a`。

没装或版本不够时 `health` 里对应的检查不过，写着安装的命令，只有方法 `yutto` 用不了；`fetch` 回 `dependency-missing`，`needs` 是 `runtime:yutto`、`runtime:ffmpeg`。清单里它们写在 `permissions.exec`，不写 `requires.runtime`：后者缺了宿主整个插件都不调，别的方法用不着它们。

登录：宿主只在这次调用里交来你登录 B站的凭据（`USUYO_CREDENTIALS`）。插件把 `SESSDATA` 写进这次调用工作目录里一个 0600 的 yutto 认证文件（`--auth-file`），用完马上删，出错也删；它不进 yutto 的参数和环境，不进日志、错误和 `detail`（交出去的文字里出现它的原样、百分号编码、解码三种写法都先换掉）。没有登录时不起 yutto，回 `login-required`。

每次调用：

```
yutto <规范链接> --video-only | --audio-only | --subtitle-only [--no-subtitle]
  --no-danmaku --no-cover --no-chapter-info --no-progress --no-color --login-strict
  -d <调用目录>/out --tmp-dir <调用目录>/tmp -tp item
  --auth-file <工作目录>/auth.toml --auth-profile default --proxy <宿主的代理或 no> --ffmpeg-path <ffmpeg>
```

- 画面、声音分两次，字幕跟在第一次里一起写（只要字幕时单独一次 `--subtitle-only`）；没有字幕的视频不算缺。
- 环境先清空；`HOME`、`XDG_CONFIG_HOME` 指到工作目录里的空目录，当前目录里没有 `yutto.toml`，读不到你自己的 yutto 配置。代理只用宿主给的（`USUYO_PROXY`，`socks5://` 改写成 `socks5h://`），没有就直连（`--proxy no`），不让 yutto 去读系统代理。
- 上限：每个文件至多 2 GiB（调用目录里任何一个文件超过就杀掉 yutto，缺的写进 `missing`），整次 55 分钟，yutto 的输出只留开头 64 KiB。
- 输出：`media/video.mp4`、`media/audio.m4a`、`media/subtitle.<语言>.srt`，名字固定；两个流都有时带同一个 `group`（`yutto:<BV>:p<分 P>`）。没有标题、作者一类，那些由 `view` 方法给。
- 错误：照 yutto 的退出码（10 HTTP 状态、11 没有权限、12 和 14 链接不对、15 和 17 找不到、16 重试用尽、18 没登录、20 合成失败）和输出里的字样；认不出的回 `upstream-changed`，提示更新 yutto。

测试用假的 yutto 和 ffmpeg（sh 脚本），夹具在 `tests/fixtures/yutto/`，都是手写的、没有测过（NOT MEASURED）。没有运行过真的 yutto，也没有向 B站发过请求。

## 不做什么

- 点赞列表、评论、弹幕。
- 字幕：`view` 方法不用账号，所以不给；要字幕用 `yutto` 方法。语音转文字由 usuyo 自己做。
- 高清画质：`view` 方法始终选能拿到的最低画质，不碰需要签名的接口；高画质交给 `yutto` 方法。
- 番剧、直播、音频区、订阅的合集。
- 登录：登录窗口和 cookie 由宿主管，插件只读宿主交来的 `USUYO_CREDENTIALS`。插件不读浏览器的 cookie 库，不保存、不打印 cookie 的值，不调 cookie 刷新接口，不带 `bili_jct`。
- cookie 只用于 `sources` 和 `list`，只发往 `api.bilibili.com`；`view` 的 `fetch`、`canonicalize` 不带 cookie，封面和视频流的请求也不带，并且只连 `*.hdslb.com`、`*.bilivideo.com`、`*.bilivideo.cn`；`yutto` 方法的登录只交给 yutto 的认证文件（见下面「yutto」）。

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
