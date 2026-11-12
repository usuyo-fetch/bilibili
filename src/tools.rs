//! 找到你装的程序，起子进程。每个子进程都有时限；到点就杀掉，留下的孙进程（yutto 起的 ffmpeg）
//! 由宿主在插件退出后对整个进程组收尾（协议 §3.13）。

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 宿主现在给的 `PATH` 只有 `${TMP}/bin` 和系统目录，还没把 `requires.runtime` 链进 `${TMP}/bin`
/// （协议 §2.13、§3.13），所以 `PATH` 之后再照宿主找 `run.command` 的顺序看这两处。
/// 宿主做了链接以后，`PATH` 里先找到的就是它链的那份。
pub const FALLBACK_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// 按顺序在这些目录里找程序。
#[derive(Debug, Clone)]
pub struct Search(Vec<PathBuf>);

impl Search {
    /// 插件进程自己的：`PATH`，再加 [`FALLBACK_DIRS`]。
    pub fn from_env() -> Self {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        dirs.extend(FALLBACK_DIRS.map(PathBuf::from));
        Self(dirs)
    }

    pub fn dirs(dirs: Vec<PathBuf>) -> Self {
        Self(dirs)
    }

    pub fn find(&self, name: &str) -> Option<PathBuf> {
        self.0
            .iter()
            .filter(|d| d.is_absolute())
            .map(|d| d.join(name))
            .find(|p| is_executable(p))
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// 子进程的一行输出从哪来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// `on_line`、`on_tick` 看过以后：接着跑，还是马上停下。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Go,
    Stop,
}

#[derive(Debug)]
pub struct Finished {
    /// `None`：被杀掉了，到点了或 `on_line`、`on_tick` 叫停了（`stopped`）。
    pub status: Option<ExitStatus>,
    /// `on_line` 或 `on_tick` 回了 [`Flow::Stop`]。
    pub stopped: bool,
    /// 留下来的 stdout（调用方说要留时）。
    pub stdout: Vec<u8>,
    /// stderr 的开头，至多 [`STDERR_KEEP`] 字节。
    pub stderr: String,
}

impl Finished {
    pub fn timed_out(&self) -> bool {
        self.status.is_none() && !self.stopped
    }

    pub fn success(&self) -> bool {
        self.status.is_some_and(|s| s.success())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("cannot start `{program}`: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("`{program}` wrote more than {limit} bytes to stdout")]
    TooMuchOutput { program: String, limit: usize },
}

/// stderr 留多少字节：失败时 `detail` 取它的开头（至多 4 KiB），多留一些好找出 `ERROR:` 那几行。
pub const STDERR_KEEP: usize = 64 << 10;
/// 一行最长读多少；更长的截掉。`-J` 的整份元数据是一行，它走 `keep_stdout` 的上限。
const LINE_BYTES: usize = 64 << 10;
/// 子进程退出以后，再等多久读完它的输出：还开着管道的孙进程不让读线程结束。
const DRAIN: Duration = Duration::from_secs(2);

/// 一次子进程的要求。
pub struct Run<'a> {
    pub program: &'a Path,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<&'a Path>,
    pub deadline: Instant,
    /// stdout 留多少字节；0 是不留，只逐行交给 `on_line`。
    pub keep_stdout: usize,
}

/// 起子进程，stdin 是空的，环境先清空再只放 `env`。每行输出交给 `on_line`，进程没退出时约每 0.1 秒
/// 叫一次 `on_tick`（量输出目录、报进度）；两个有一个回 [`Flow::Stop`] 就杀掉。
/// 到 `deadline` 还没退出也杀掉（`status: None`）。
pub fn run(
    req: Run<'_>,
    mut on_line: impl FnMut(Stream, &str) -> Flow,
    mut on_tick: impl FnMut() -> Flow,
) -> Result<Finished, RunError> {
    let program = req.program.display().to_string();
    let mut command = Command::new(req.program);
    command
        .args(&req.args)
        .env_clear()
        .envs(req.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = req.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|source| RunError::Spawn {
        program: program.clone(),
        source,
    })?;
    let (tx, rx) = mpsc::channel::<(Stream, Vec<u8>)>();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        let whole = req.keep_stdout > 0;
        std::thread::spawn(move || read_lines(out, Stream::Stdout, whole, &tx));
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || read_lines(err, Stream::Stderr, false, &tx));
    }
    drop(tx);

    let mut stdout = Vec::new();
    let mut stderr = String::new();
    let mut exited: Option<(ExitStatus, Instant)> = None;
    let mut too_much = false;
    let mut stopped = false;
    loop {
        let now = Instant::now();
        if exited.is_none()
            && let Ok(Some(status)) = child.try_wait()
        {
            exited = Some((status, now));
        }
        let until = match exited {
            Some((_, at)) => at + DRAIN,
            None => req.deadline,
        };
        if now >= until {
            break;
        }
        if exited.is_none() && on_tick() == Flow::Stop {
            stopped = true;
            break;
        }
        let wait = until
            .saturating_duration_since(now)
            .min(Duration::from_millis(100));
        match rx.recv_timeout(wait) {
            Ok((stream, bytes)) => {
                if stream == Stream::Stdout && req.keep_stdout > 0 {
                    if stdout.len() + bytes.len() > req.keep_stdout {
                        too_much = true;
                        break;
                    }
                    stdout.extend_from_slice(&bytes);
                }
                let text = String::from_utf8_lossy(&bytes);
                let line = text.trim_end_matches(['\r', '\n']);
                if stream == Stream::Stderr && stderr.len() < STDERR_KEEP {
                    let room = STDERR_KEEP - stderr.len();
                    let mut end = line.len().min(room.saturating_sub(1));
                    while !line.is_char_boundary(end) {
                        end -= 1;
                    }
                    stderr.push_str(&line[..end]);
                    stderr.push('\n');
                }
                if !(stream == Stream::Stdout && req.keep_stdout > 0)
                    && on_line(stream, line) == Flow::Stop
                {
                    stopped = exited.is_none();
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // 两个读线程都结束了：等进程退出
            Err(mpsc::RecvTimeoutError::Disconnected) => match exited {
                Some(_) => break,
                None => {
                    if let Ok(Some(status)) = child.try_wait() {
                        exited = Some((status, Instant::now()));
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            },
        }
    }
    let status = match exited {
        Some((status, _)) => Some(status),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    };
    if too_much {
        let _ = child.kill();
        let _ = child.wait();
        return Err(RunError::TooMuchOutput {
            program,
            limit: req.keep_stdout,
        });
    }
    Ok(Finished {
        status,
        stopped,
        stdout,
        stderr,
    })
}

/// 逐行（`\n` 或进度条用的 `\r`）读，每行至多 [`LINE_BYTES`]；`whole` 时整段交出、不分行。
fn read_lines(
    source: impl Read,
    stream: Stream,
    whole: bool,
    tx: &mpsc::Sender<(Stream, Vec<u8>)>,
) {
    let mut reader = BufReader::new(source);
    if whole {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send((stream, buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    }
    let mut line = Vec::new();
    loop {
        let (done, used) = match reader.fill_buf() {
            Ok([]) | Err(_) => (true, 0),
            Ok(chunk) => {
                let mut used = 0;
                for &b in chunk {
                    used += 1;
                    if b == b'\n' || b == b'\r' {
                        if !line.is_empty() && tx.send((stream, std::mem::take(&mut line))).is_err()
                        {
                            return;
                        }
                    } else if line.len() < LINE_BYTES {
                        line.push(b);
                    }
                }
                (false, used)
            }
        };
        reader.consume(used);
        if done {
            if !line.is_empty() {
                let _ = tx.send((stream, line));
            }
            return;
        }
    }
}

/// 跑 `<program> <flag>`，取第一行，至多等 `timeout`。拿不到回 `Err`（一句英文原因）。
pub fn version(program: &Path, flag: &str, timeout: Duration) -> Result<String, String> {
    let finished = run(
        Run {
            program,
            args: vec![flag.to_owned()],
            env: base_env(),
            cwd: None,
            deadline: Instant::now() + timeout,
            keep_stdout: 64 << 10,
        },
        |_, _| Flow::Go,
        || Flow::Go,
    )
    .map_err(|e| e.to_string())?;
    if finished.timed_out() {
        return Err(format!("did not answer `{flag}` within {timeout:?}"));
    }
    if !finished.success() {
        let why = finished
            .stderr
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        return Err(format!("`{flag}` failed: {why}"));
    }
    String::from_utf8_lossy(&finished.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("`{flag}` printed nothing"))
}

/// 给子进程的环境：只有宿主给插件的这几个目录变量，加上 UTF-8。
/// 不传 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 一类：路线只由 `--proxy` 定。
pub fn base_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = [
        "PATH",
        "HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "SYSTEMROOT",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_owned(), v)))
    .collect();
    env.extend(
        [
            ("LANG", "en_US.UTF-8"),
            ("PYTHONUTF8", "1"),
            ("PYTHONIOENCODING", "utf-8"),
            ("PYTHONDONTWRITEBYTECODE", "1"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    env
}
