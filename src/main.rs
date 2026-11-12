//! `usuyo-fetch-bilibili`：子命令照协议 §3.2，stdout 只写一个信封。

use std::process::ExitCode;

use anyhow::Context;
use serde::Serialize;
use usuyo_fetch_bilibili::api::{Bilibili, Endpoints, SITE};
use usuyo_fetch_bilibili::session::Session;
use usuyo_fetch_bilibili::{canonical, favorites, video};
use usuyo_plugin_kit::fetch::{Call, USAGE};
use usuyo_plugin_kit::protocol::{
    CallFailure, Envelope, ErrorCode, FetchHealth, LoginReport, LoginReportState,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::from(1)
        }
    }
}

/// 分派到清单里的方法：`view` 取单条，`favorites-public` 列公开收藏夹（不带 cookie），
/// `favorites` 列我的收藏夹和稍后再看（带 cookie）。浏览器脚本不在这里跑，`yutto` 还没做。
fn run(args: &[String]) -> anyhow::Result<u8> {
    let anonymous = || Bilibili::new(Endpoints::production(), None);
    let with_session = || -> Result<Bilibili, CallFailure> {
        Ok(Bilibili::new(Endpoints::production(), Session::from_env()?))
    };
    let Some(call) = Call::parse(args) else {
        return usage();
    };
    let method = call.method().map(str::to_owned);
    match (method.as_deref(), call) {
        (_, Call::Canonicalize { url }) => emit(canonical::canonicalize(&anonymous(), &url)),
        (
            Some("view"),
            Call::Fetch {
                input, out, want, ..
            },
        ) => emit(video::fetch(&anonymous(), &input, &out, want.as_deref())),
        (
            Some("favorites-public"),
            Call::Sources {
                link: Some(link), ..
            },
        ) => emit(favorites::public_sources(&anonymous(), &link)),
        (Some("favorites-public"), Call::List { source, cursor, .. }) => emit(
            favorites::public_list(&anonymous(), &source, cursor.as_deref()),
        ),
        (Some("favorites"), Call::Sources { link: None, .. }) => {
            emit(with_session().and_then(|api| favorites::sources(&api)))
        }
        (
            Some("favorites"),
            Call::List {
                source,
                cursor,
                since,
                ..
            },
        ) => emit(
            with_session()
                .and_then(|api| favorites::list(&api, &source, cursor.as_deref(), since.as_ref())),
        ),
        (_, Call::Health { probe: false }) => emit(Ok(health(None))),
        (_, Call::Health { probe: true }) => {
            emit(with_session().and_then(|api| probe(&api)).map(health))
        }
        (method, _) => emit::<()>(Err(CallFailure::new(
            ErrorCode::InvalidArgs,
            format!("this call does not apply to method {method:?}"),
        ))),
    }
}

fn usage() -> anyhow::Result<u8> {
    eprintln!("{USAGE}");
    Ok(2)
}

/// 把结果写成 stdout 上的那一个信封，返回退出码。
fn emit<T: Serialize>(result: Result<T, CallFailure>) -> anyhow::Result<u8> {
    let envelope = match result {
        Ok(body) => Envelope::Ok(body),
        Err(failure) => Envelope::Err(failure),
    };
    println!(
        "{}",
        serde_json::to_string(&envelope).context("cannot write the envelope")?
    );
    Ok(u8::from(envelope.exit_code() != 0))
}

/// `health --probe`：用一次不签名的 `nav` 请求核对登录。没有登录信息就不发请求。
fn probe(api: &Bilibili) -> Result<Option<LoginReportState>, CallFailure> {
    if api.session().is_none() {
        return Ok(None);
    }
    match api.get("x/web-interface/nav", &[]) {
        Ok(reply) => Ok(Some(if reply.data["isLogin"].as_bool() == Some(true) {
            LoginReportState::Verified
        } else {
            LoginReportState::Rejected
        })),
        Err(e) if e.code() == Some(-101) => Ok(Some(LoginReportState::Rejected)),
        Err(e) => Err(e.into()),
    }
}

/// 登录状态（`present`、`missing`、`expired`）由宿主按 `sites.cookies` 算，
/// 这里只在 `--probe` 核对过以后报 `verified` 或 `rejected`。没有要查的本机程序。
fn health(probed: Option<LoginReportState>) -> FetchHealth {
    let mut h = FetchHealth::new(env!("CARGO_PKG_VERSION"));
    h.login.extend(probed.map(|state| LoginReport {
        site: SITE.into(),
        state,
    }));
    h
}
