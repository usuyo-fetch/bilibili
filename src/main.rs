//! `usuyo-fetch-bilibili`：子命令照协议 §3.2，stdout 只写一个信封。

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use serde::Serialize;
use serde_json::{Value, json};
use usuyo_fetch_bilibili::api::{ApiError, Bilibili, Endpoints, SITE};
use usuyo_fetch_bilibili::session::Session;
use usuyo_fetch_bilibili::{canonical, favorites, video};
use usuyo_plugin_kit::protocol::{CallFailure, Envelope, OutputKind, Rfc3339};

const USAGE: &str = "usage: usuyo-fetch-bilibili canonicalize <url> | fetch <url> --out <dir> [--want a,b] | sources | list <source> [--cursor <c>] [--since <time>] | health [--probe]";

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

fn run(args: &[String]) -> anyhow::Result<u8> {
    let anonymous = || Bilibili::new(Endpoints::production(), None);
    let with_session = || -> Result<Bilibili, CallFailure> {
        Ok(Bilibili::new(Endpoints::production(), Session::from_env()?))
    };
    match args.first().map(String::as_str) {
        Some("canonicalize") if args.len() == 2 => {
            emit(canonical::canonicalize(&anonymous(), &args[1]))
        }
        Some("fetch") => match parse_fetch(&args[1..]) {
            Some((input, out, want)) => emit(
                with_session().and_then(|api| video::fetch(&api, &input, &out, want.as_deref())),
            ),
            None => usage(),
        },
        Some("sources") if args.len() == 1 => {
            emit(with_session().and_then(|api| favorites::sources(&api)))
        }
        Some("list") => {
            match parse_list(&args[1..]) {
                Some((source, cursor, since)) => emit(with_session().and_then(|api| {
                    favorites::list(&api, &source, cursor.as_deref(), since.as_ref())
                })),
                None => usage(),
            }
        }
        Some("health") if args.len() == 1 => emit(Ok(health(None))),
        Some("health") if args.len() == 2 && args[1] == "--probe" => {
            emit(with_session().and_then(|api| probe(&api)).map(health))
        }
        _ => usage(),
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

fn parse_fetch(args: &[String]) -> Option<(String, PathBuf, Option<Vec<OutputKind>>)> {
    let (input, rest) = args.split_first()?;
    let mut out = None;
    let mut want = None;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--out" => out = Some(PathBuf::from(it.next()?)),
            "--want" => {
                want = Some(
                    it.next()?
                        .split(',')
                        .map(OutputKind::parse)
                        .collect::<Option<Vec<_>>>()?,
                );
            }
            _ => return None,
        }
    }
    Some((input.clone(), out?, want))
}

fn parse_list(args: &[String]) -> Option<(String, Option<String>, Option<Rfc3339>)> {
    let (source, rest) = args.split_first()?;
    let mut cursor = None;
    let mut since = None;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--cursor" => cursor = Some(it.next()?.clone()),
            "--since" => since = Some(Rfc3339::try_from(it.next()?.clone()).ok()?),
            _ => return None,
        }
    }
    Some((source.clone(), cursor, since))
}

/// `health --probe`：用一次不签名的 `nav` 请求核对登录。没有登录信息就不发请求。
fn probe(api: &Bilibili) -> Result<Option<&'static str>, CallFailure> {
    if api.session().is_none() {
        return Ok(None);
    }
    match api.get("x/web-interface/nav", &[]) {
        Ok(reply) => Ok(Some(if reply.data["isLogin"].as_bool() == Some(true) {
            "verified"
        } else {
            "rejected"
        })),
        Err(ApiError::Code { code: -101, .. }) => Ok(Some("rejected")),
        Err(e) => Err(e.into()),
    }
}

/// 登录状态（`present`、`missing`、`expired`）由宿主按 `sites.cookies` 算，
/// 这里只在 `--probe` 核对过以后报 `verified` 或 `rejected`。
fn health(probed: Option<&'static str>) -> Value {
    let login: Vec<Value> = probed
        .map(|state| json!({"site": SITE, "state": state}))
        .into_iter()
        .collect();
    json!({
        "ready": true,
        "protocol": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "upstream": [],
        "login": login,
        "checks": []
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn fetch_args() {
        let (input, out, want) = parse_fetch(&strings(&[
            "u",
            "--out",
            "/o",
            "--want",
            "text,media:video",
        ]))
        .unwrap();
        assert_eq!((input.as_str(), out), ("u", PathBuf::from("/o")));
        assert_eq!(want, Some(vec![OutputKind::Text, OutputKind::Video]));
        assert!(parse_fetch(&strings(&["u"])).is_none());
        assert!(parse_fetch(&strings(&["u", "--out", "/o", "--want", "nope"])).is_none());
    }

    #[test]
    fn list_args() {
        let (source, cursor, since) = parse_list(&strings(&[
            "bilibili:toview",
            "--cursor",
            "3",
            "--since",
            "2026-10-31T00:00:00Z",
        ]))
        .unwrap();
        assert_eq!(
            (source.as_str(), cursor.as_deref()),
            ("bilibili:toview", Some("3"))
        );
        assert!(since.is_some());
        assert!(parse_list(&strings(&["s", "--since", "yesterday"])).is_none());
    }
}
