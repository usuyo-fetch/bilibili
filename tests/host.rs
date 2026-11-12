//! 真的二进制：不需要网络的子命令，以及清单经宿主的校验和调用。

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use usuyo_plugin::protocol::{
    Access, Account, Canonicalized, ErrorCode, Identity, ListOrder, Login,
};
use usuyo_plugin::{
    CallLimits, CallOptions, HostConfig, ListRequest, PluginError, PluginHost, discover,
};

const BIN: &str = env!("CARGO_BIN_EXE_usuyo-fetch-bilibili");

/// 清空环境后运行，和宿主一样；`env` 是额外传入的变量。
fn run(args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
    let output = Command::new(BIN)
        .args(args)
        .env_clear()
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json = if stdout.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&stdout).unwrap()
    };
    (output.status.code().unwrap(), json)
}

#[test]
fn canonicalize_needs_no_network_for_ids() {
    let (code, out) = run(
        &[
            "canonicalize",
            "https://www.bilibili.com/video/av170001?p=2",
        ],
        &[],
    );
    assert_eq!(code, 0);
    assert_eq!(out["ok"], true);
    assert_eq!(out["match"], true);
    assert_eq!(out["identity"]["platform"], "bilibili");
    assert_eq!(out["identity"]["contentId"], "BV17x411w7KC:p2");
    assert_eq!(
        out["identity"]["url"],
        "https://www.bilibili.com/video/BV17x411w7KC?p=2"
    );
    assert!(out.get("resolvedUrl").is_none());

    let (code, out) = run(
        &["canonicalize", "https://example.com/video/BV17x411w7KC"],
        &[],
    );
    assert_eq!((code, &out["match"]), (0, &Value::Bool(false)));
}

#[test]
fn health_does_not_touch_the_network() {
    let (code, out) = run(&["health"], &[]);
    assert_eq!(code, 0);
    assert_eq!(
        (out["ready"].as_bool(), out["protocol"].as_u64()),
        (Some(true), Some(1))
    );
    assert_eq!(out["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(out["login"], serde_json::json!([]));
    // 没有登录信息时，--probe 也不发请求
    let (code, out) = run(&["health", "--probe"], &[]);
    assert_eq!((code, &out["login"]), (0, &serde_json::json!([])));
}

#[test]
fn login_only_commands_fail_cleanly_without_credentials() {
    for args in [
        &["sources", "--method", "favorites"][..],
        &["list", "bilibili:toview", "--method", "favorites"][..],
    ] {
        let (code, out) = run(args, &[]);
        assert_eq!(code, 1, "{args:?}");
        assert_eq!(out["ok"], false);
        assert_eq!(out["error"]["code"], "login-required");
        assert_eq!(out["error"]["site"], "www.bilibili.com");
    }
}

#[test]
fn a_bad_credentials_file_is_an_internal_error_without_its_content() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("credentials.json");
    std::fs::write(
        &path,
        r#"{"storageState":{"cookies":[{"name":"SESSDATA","value":12345678}]}}"#,
    )
    .unwrap();
    let (code, out) = run(
        &["sources", "--method", "favorites"],
        &[("USUYO_CREDENTIALS", path.to_str().unwrap())],
    );
    assert_eq!(code, 1);
    assert_eq!(out["error"]["code"], "internal");
    assert!(!out.to_string().contains("12345678"));
}

/// 凭据文件指向一个不存在的路径：读它的子命令会报凭据错误，fetch 不读，照常往下走。
#[test]
fn fetch_does_not_open_the_credentials_file() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no-such-credentials.json");
    let env = [("USUYO_CREDENTIALS", missing.to_str().unwrap())];
    let (code, out) = run(&["sources", "--method", "favorites"], &env);
    assert_eq!((code, &out["error"]["code"]), (1, &Value::from("internal")));
    // 公开收藏夹不读凭据：认不出的链接在发请求之前就回
    let (code, out) = run(
        &[
            "sources",
            "--method",
            "favorites-public",
            "--link",
            "https://example.com/u/1",
        ],
        &env,
    );
    assert_eq!(
        (code, &out["error"]["code"]),
        (1, &Value::from("unsupported-input"))
    );

    // 不是 B站的链接：不发请求，在读到凭据之前就能判定
    let (code, out) = run(
        &[
            "fetch",
            "https://example.com/x",
            "--out",
            tmp.path().to_str().unwrap(),
            "--method",
            "view",
        ],
        &env,
    );
    assert_eq!(code, 1);
    assert_eq!(out["error"]["code"], "unsupported-input");
}

#[test]
fn bad_usage_exits_2() {
    for args in [
        &[][..],
        &["nope"][..],
        &["fetch", "x"][..],
        &["fetch", "x", "--out", "/o"][..],
        &["sources"][..],
        &["canonicalize"][..],
    ] {
        let (code, out) = run(args, &[]);
        assert_eq!((code, out), (2, Value::Null), "{args:?}");
    }
}

#[test]
fn each_call_goes_to_its_method() {
    // 方法和调用对不上的回 invalid-args，不发请求
    for args in [
        &["sources", "--method", "view"][..],
        &["sources", "--method", "favorites-public"][..],
        &[
            "sources",
            "--method",
            "favorites",
            "--link",
            "https://space.bilibili.com/1",
        ][..],
        &["list", "bilibili:fav-public:1", "--method", "userscript"][..],
        &[
            "fetch",
            "https://www.bilibili.com/video/BV17x411w7KC",
            "--out",
            "/o",
            "--method",
            "favorites",
        ][..],
        &[
            "fetch",
            "https://www.bilibili.com/video/BV17x411w7KC",
            "--out",
            "/o",
            "--method",
            "yutto",
        ][..],
    ] {
        let (code, out) = run(args, &[]);
        assert_eq!(
            (code, &out["error"]["code"]),
            (1, &Value::from("invalid-args")),
            "{args:?}"
        );
    }
    // 公开收藏夹的来源只认自己的前缀，不发请求
    let (code, out) = run(
        &["list", "bilibili:fav:1", "--method", "favorites-public"],
        &[],
    );
    assert_eq!(
        (code, &out["error"]["code"]),
        (1, &Value::from("unsupported-input"))
    );
}

/// 清单装成宿主认的样子：`<root>/usuyo-fetch/bilibili/<version>/`。
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
async fn the_manifest_loads_and_matches_bilibili_links() {
    let (_tmp, host) = host();
    let plugin = &host.plugins()[0];
    assert_eq!(plugin.id().as_str(), "usuyo-fetch/bilibili");
    assert_eq!(plugin.header.version.to_string(), env!("CARGO_PKG_VERSION"));
    let spec = &plugin.spec;
    // routes.md 第 5 节的每一行（client-hook 那行在另一个组织），顺序照清单
    let ids: Vec<&str> = spec.methods.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "view",
            "favorites-public",
            "favorites",
            "userscript",
            "yutto"
        ]
    );
    let view = spec.method("view").unwrap();
    assert_eq!(
        (view.access, view.login, view.account),
        (Access::PublicWeb, Login::None, Account::None)
    );
    assert!(view.sites.is_empty() && view.capabilities.fetch.is_some());
    let public = spec.method("favorites-public").unwrap();
    assert!(public.sites.is_empty() && public.on_by_default());
    let public_list = public.capabilities.list.as_ref().unwrap();
    assert!(public_list.link.is_some() && public_list.sources.is_empty());
    // 实测一页里从旧到新：不说 newest-first
    assert_eq!(public_list.order, ListOrder::Unordered);
    let mine = spec.method("favorites").unwrap();
    assert_eq!(
        (mine.access, mine.login, mine.account),
        (Access::SessionApi, Login::Required, Account::InUsuyo)
    );
    assert_eq!(mine.sites, ["www.bilibili.com"]);
    assert_eq!(
        mine.capabilities.list.as_ref().unwrap().order,
        ListOrder::NewestFirst
    );
    assert!(spec.method("userscript").unwrap().delivers.is_some());
    assert!(!spec.method("yutto").unwrap().built);
    assert_eq!(
        (spec.access(), spec.login()),
        (Access::SessionApi, Login::Required)
    );
    assert_eq!(plugin.spec.sites[0].cookies, ["SESSDATA"]);
    for hit in [
        "https://www.bilibili.com/video/BV17x411w7KC",
        "https://m.bilibili.com/video/av170001",
        "https://b23.tv/AbCdEf",
    ] {
        let url = url::Url::parse(hit).unwrap();
        assert_eq!(host.candidates(&url).len(), 1, "{hit}");
    }
    let url = url::Url::parse("https://www.bilibili.com/bangumi/play/ep1").unwrap();
    assert!(host.candidates(&url).is_empty());

    // 经宿主调一次：输出要过宿主的 schema 校验
    let url = url::Url::parse("https://www.bilibili.com/video/av170001").unwrap();
    let canonical = host
        .canonicalize(host.candidates(&url)[0], &url, &CallOptions::default())
        .await
        .unwrap();
    let Canonicalized::Match {
        identity: Identity::Platform { content_id, .. },
        ..
    } = canonical
    else {
        panic!("{canonical:?}")
    };
    assert_eq!(content_id, "BV17x411w7KC");
}

#[tokio::test]
async fn list_without_login_is_reported_through_the_host() {
    let (_tmp, host) = host();
    let plugin = &host.plugins()[0];
    let err = host
        .sources(plugin, "favorites", None, &CallOptions::default())
        .await
        .unwrap_err();
    let PluginError::Call { failure, .. } = err else {
        panic!("{err:?}")
    };
    assert_eq!(failure.code, ErrorCode::LoginRequired);
    let request = ListRequest {
        method: "favorites".into(),
        source: "bilibili:toview".into(),
        cursor: None,
        since: None,
        input_file: None,
    };
    let err = host
        .list(plugin, request, &CallOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, PluginError::Call { .. }), "{err:?}");
}
