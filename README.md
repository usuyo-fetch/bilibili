# usuyo-fetch/bilibili

B站的获取插件，供 [usuyo](https://github.com/usuyo/usuyo) 调用：取回单个视频，列出收藏夹和稍后再看。

| 功能 | 实现方式（方法 id） | 账号 | 风险层级 | 状态 |
|---|---|---|---|---|
| 按链接取单条内容（元数据、封面、低画质音视频） | `view` | 不用 | `public-web`，默认开 | 已实现（2026-11-12 不带账号对真实 B站 跑过一次） |
| 列出公开收藏夹 | `favorites-public` | 不用 | `public-web`，默认开 | 已实现（2026-11-12 不带账号对真实 B站 跑过一次） |
| 列出我的收藏夹、稍后再看 | `favorites` | 在 usuyo 里登录的 B站 | `session-api`，默认开 | 已实现（对着本机假服务测过，没有对着真实 B站 跑过） |
| 列出我的收藏夹、稍后再看 | `userscript` | 只在你的浏览器里 | `browser-session`，默认开 | 已实现（脚本测试对着手写样例，没有在真浏览器里跑过） |

## 方法

### `view`

`fetch`：元数据（`view`）、封面、按需要的低画质音视频流。不读凭据、不带 cookie，即使你登录过。当前以取到内容为先，不追画质；高清视频是试验性的，还没有实现。

### `favorites-public`

`sources --link`：给一个人的个人空间（`space.bilibili.com/<mid>`）就列他公开的收藏夹，给一个收藏夹的链接就是它。`list`：翻一个公开收藏夹，一页 20 条，`actedAt` 是收藏时间；2026-11-12 实测一页里从旧到新，清单写 `unordered`。

### `favorites`

`sources`：你自己的收藏夹和「稍后再看」，mid 取自 cookie `DedeUserID`。`list`：收藏夹按页码翻页，稍后再看一次给完，按加入时间从新到旧。

### `userscript`

不在插件里跑：`userscript/` 里的脚本在你的浏览器里把收藏夹、稍后再看交给 usuyo。

## 子命令

| 子命令 | 说明 |
|---|---|
| `canonicalize` | 认出 BV 号、av 号、`b23.tv` 短链和分 P（`BV…:p2`）。av 号离线换成 BV 号；只有短链会发请求，只跟随重定向，不带 cookie。不分方法 |
| `health` | 不发请求，只报插件版本；`--probe` 才用登录信息请求一次 `nav` |

## 测试

`cargo test`，对着本机假服务；`view.json` 是一次真实响应，其余样例手写。

## 参考的项目

接口字段名参考了 bili-sync（MIT），接口参数参考了 yutto 的做法；没有调用它们，也没有移植它们的代码。许可证：MIT。

## 开发

`Cargo.toml` 里的 `usuyo-plugin`、`usuyo-plugin-kit` 是对 `https://github.com/usuyo/usuyo` 的 git 依赖，钉在固定的 `rev`。

本地开发时用旁边 `usuyo/` 的工作区代替它们：`usuyo-fetch/.cargo/config.toml`（不进任何插件仓库）写了下面的覆盖，cargo 从当前目录往上找这个文件，在插件文件夹里跑 cargo 就生效；从别处跑要加 `--config <这个文件>`。

```toml
[patch."https://github.com/usuyo/usuyo"]
usuyo-plugin = { path = "../usuyo/crates/usuyo-plugin" }
usuyo-plugin-kit = { path = "../usuyo/crates/usuyo-plugin-kit" }
```

`Cargo.lock` 记的是覆盖后的路径形式，所以 CI 不加 `--locked` 构建，cargo 只重新锁这两个 usuyo crate。
