# usuyo-fetch/bilibili

B站的获取插件，供 [usuyo](https://github.com/usuyo/usuyo) 调用：取回单个视频，列出收藏夹和稍后再看。

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

测试：`cargo test`，对着本机假服务；`view.json` 是一次真实响应，其余样例手写。`yutto` 方法需要自己装 [yutto](https://github.com/yutto-dev/yutto)（GPL-3.0，独立进程）和 ffmpeg。字段名取自 bili-sync（MIT），接口参数对照 yutto。许可证：MIT。
