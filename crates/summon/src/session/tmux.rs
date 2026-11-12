use std::collections::BTreeSet;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use summon_core::web_terminal::scroll::{PaneMouse, WheelEvents};

const HANDOFF_STAGING_PREFIX: &str = "summon-bridge-handoff-";
const HANDOFF_STAGING_OWNER_ENV: &str = "SUMMON_BRIDGE_STAGING_OWNER";
/// `send_bytes` 每次 tmux 调用最多带这么多字节(每字节一个 argv 项)。
const SEND_BYTES_CHUNK: usize = 1024;

/// tmux 自己分配的稳定身份。rename 不改变 id/created/pane；cleanup 前必须逐项复验，不能只信名字。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmuxSessionIdentity {
    pub(crate) name: String,
    pub(crate) session_id: String,
    pub(crate) session_created: u64,
    pub(crate) pane_id: String,
    pub(crate) ownership_marker: String,
}

/// 只代表本次调用成功创建并写入 ownership marker 的 handoff staging。
/// 不实现 Drop 自动清理：Drop 时 IO 失败不可见，且 rename 后绝不能误杀 canonical target。
pub(crate) struct OwnedHandoffStaging {
    identity: TmuxSessionIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RenamedHandoffTmux {
    pub(crate) identity: TmuxSessionIdentity,
}

/// 统一 resolver 只读进程启动快照；不会在 tmux 热路径反复加载 config。
fn tmux_bin() -> PathBuf {
    summon_core::tools::resolve(summon_core::tools::Tool::Tmux)
}

impl OwnedHandoffStaging {
    pub(crate) fn identity(&self) -> &TmuxSessionIdentity {
        &self.identity
    }

    pub(crate) fn owns_process(&self, pid: u32) -> io::Result<bool> {
        let current =
            inspect_handoff_identity(&self.identity.name, &self.identity.ownership_marker)?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, summon_i18n::text!("r04d4f4a12e"))
                })?;
        if current != self.identity {
            return Err(io::Error::other(summon_i18n::text!("rd79d2b274e")));
        }
        let output = Command::new(tmux_bin())
            .args([
                "display-message",
                "-p",
                "-t",
                &format!("={}:", self.identity.name),
                "-F",
                "#{pane_pid}",
            ])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(summon_i18n::text!("r839ae7f95a")));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim() == pid.to_string())
    }

    /// 恢复 daemon 重启前创建的 staging。名字、tmux identity 和 marker 全匹配才返回所有权。
    pub(crate) fn recover(expected: &TmuxSessionIdentity) -> io::Result<Self> {
        validate_staging_name(&expected.name)?;
        let current = inspect_handoff_identity(&expected.name, &expected.ownership_marker)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, summon_i18n::text!("r04d4f4a12e"))
            })?;
        if current != *expected {
            return Err(io::Error::other(summon_i18n::text!("rd79d2b274e")));
        }
        Ok(Self { identity: current })
    }

    pub(crate) fn recover_named(name: &str, ownership_marker: &str) -> io::Result<Option<Self>> {
        validate_staging_name(name)?;
        inspect_handoff_identity(name, ownership_marker)
            .map(|identity| identity.map(|identity| Self { identity }))
    }

    /// 仅清理由本类型持有且 identity 未变化的 staging；失败时保留会话并向上传错。
    pub(crate) fn cleanup(self) -> io::Result<()> {
        let current =
            inspect_handoff_identity(&self.identity.name, &self.identity.ownership_marker)?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, summon_i18n::text!("r04d4f4a12e"))
                })?;
        if current != self.identity {
            return Err(io::Error::other(summon_i18n::text!("rc54636e4cc")));
        }
        let status = Command::new(tmux_bin())
            .args([
                "kill-session",
                "-t",
                &exact_session_target(&self.identity.name),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() || has_session_strict(&self.identity.name)? {
            return Err(io::Error::other(summon_i18n::text!("ra5f725a67b")));
        }
        Ok(())
    }

    /// rename 前复验 staging identity 且要求 canonical target 不存在；成功后 identity 除名字外不变。
    pub(crate) fn rename_exact(self, target: &str) -> io::Result<RenamedHandoffTmux> {
        validate_tmux_name(target)?;
        if target.starts_with(HANDOFF_STAGING_PREFIX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                summon_i18n::text!("rf15f5679a0"),
            ));
        }
        let current =
            inspect_handoff_identity(&self.identity.name, &self.identity.ownership_marker)?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, summon_i18n::text!("r04d4f4a12e"))
                })?;
        if current != self.identity {
            return Err(io::Error::other(summon_i18n::text!("rd79d2b274e")));
        }
        if has_session_strict(target)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                summon_i18n::text!("r2717600b46"),
            ));
        }
        let status = Command::new(tmux_bin())
            .args([
                "rename-session",
                "-t",
                &exact_session_target(&self.identity.name),
                target,
            ])
            .status()?;
        if !status.success() {
            return Err(io::Error::other(summon_i18n::text!("r6af6d2d1f5")));
        }
        let renamed = inspect_handoff_identity(target, &self.identity.ownership_marker)?
            .ok_or_else(|| io::Error::other(summon_i18n::text!("r79dc81693d")))?;
        if renamed.session_id != self.identity.session_id
            || renamed.session_created != self.identity.session_created
            || renamed.pane_id != self.identity.pane_id
        {
            return Err(io::Error::other(summon_i18n::text!("r7ebe487e71")));
        }
        Ok(RenamedHandoffTmux { identity: renamed })
    }
}

impl RenamedHandoffTmux {
    pub(crate) fn identity(&self) -> &TmuxSessionIdentity {
        &self.identity
    }

    pub(crate) fn recover(staging: &TmuxSessionIdentity, target: &str) -> io::Result<Self> {
        validate_tmux_name(target)?;
        let current =
            inspect_handoff_identity(target, &staging.ownership_marker)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, summon_i18n::text!("rb5704c59de"))
            })?;
        if current.session_id != staging.session_id
            || current.session_created != staging.session_created
            || current.pane_id != staging.pane_id
        {
            return Err(io::Error::other(summon_i18n::text!("r7e8eb3b1be")));
        }
        Ok(Self { identity: current })
    }

    #[cfg(test)]
    fn cleanup_test_target(self) -> io::Result<()> {
        if !self.identity.name.starts_with("summon-bridge-test-") {
            return Err(io::Error::other(summon_i18n::text!("r50fca51ae9")));
        }
        let current =
            inspect_handoff_identity(&self.identity.name, &self.identity.ownership_marker)?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, summon_i18n::text!("r86e80a05a4"))
                })?;
        if current != self.identity {
            return Err(io::Error::other(summon_i18n::text!("rd3c2f02c67")));
        }
        let status = Command::new(tmux_bin())
            .args([
                "kill-session",
                "-t",
                &exact_session_target(&self.identity.name),
            ])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(summon_i18n::text!("r8020c6b139")))
        }
    }
}

/// staging 的 `tmux new-session` argv(纯件)。program/args 逐项走 argv、不进 shell 解析;
/// **环境不进 argv** —— 只留一次性 env 文件的路径,由 `ENV_FILE_LOADER_SCRIPT` 在 pane 里
/// `set -a` 载入后立刻删除,再 `exec` program。`env -i` 仍在最前,清空继承语义不变;
/// `env`→`sh`→program 全是 exec 链,pane_pid 仍等于 program 本身的 pid(owns_process 依赖这条)。
fn staging_new_session_argv(
    name: &str,
    cwd: &Path,
    env_file: &Path,
    program: &Path,
    args: &[String],
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let format = "#{session_name},#{session_id},#{session_created},#{pane_id}";
    let mut argv: Vec<OsString> = ["new-session", "-d", "-P", "-F", format, "-s", name, "-c"]
        .iter()
        .map(OsString::from)
        .collect();
    argv.push(cwd.into());
    for fixed in [
        "--",
        "/usr/bin/env",
        "-i",
        "/bin/sh",
        "-c",
        summon_core::launch_env::ENV_FILE_LOADER_SCRIPT,
    ] {
        argv.push(OsString::from(fixed));
    }
    argv.push(env_file.into());
    argv.push(program.into());
    argv.extend(args.iter().map(OsString::from));
    argv
}

/// argv 逐项传给 tmux，不构造 shell command string；环境经一次性 0600 env 文件送达（不进 argv）。
/// 返回值是后续唯一合法 cleanup 能力。
pub(crate) fn create_handoff_staging(
    name: &str,
    ownership_marker: &str,
    cwd: &Path,
    program: &Path,
    args: &[String],
    env: &[(String, String)],
) -> io::Result<OwnedHandoffStaging> {
    validate_staging_name(name)?;
    validate_marker(ownership_marker)?;
    if !cwd.is_absolute() || !program.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::text!("rae8186192b"),
        ));
    }
    if has_session_strict(name)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            summon_i18n::text!("r889a2fe5c1"),
        ));
    }
    for (key, value) in env {
        validate_env(key, value)?;
    }

    // staging 的 env 含一次性 handoff token,绝不能留在盘上:文件 0600、pane 载入后立刻自删;
    // 无论走哪条返回路径这里都兜底再删一次(pane 没起来就没人替我们删;已删则 no-op)。
    let env_file = staging_env_file_path(name);
    write_launch_env_file(&env_file, env)?;
    let launched = launch_staging_session(name, ownership_marker, cwd, &env_file, program, args);
    let _ = std::fs::remove_file(&env_file);
    launched
}

fn launch_staging_session(
    name: &str,
    ownership_marker: &str,
    cwd: &Path,
    env_file: &Path,
    program: &Path,
    args: &[String],
) -> io::Result<OwnedHandoffStaging> {
    let output = Command::new(tmux_bin())
        .args(staging_new_session_argv(name, cwd, env_file, program, args))
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(summon_i18n::text!("rf07a5799a4")));
    }
    let mut identity = parse_identity(&String::from_utf8_lossy(&output.stdout), ownership_marker)?;
    if identity.name != name {
        let _ = cleanup_unmarked_created_session(name);
        return Err(io::Error::other(summon_i18n::text!("rf0d75deeae")));
    }
    let marker_status = Command::new(tmux_bin())
        .args([
            "set-environment",
            "-t",
            &exact_session_target(name),
            HANDOFF_STAGING_OWNER_ENV,
            ownership_marker,
        ])
        .status()?;
    if !marker_status.success() {
        let _ = cleanup_unmarked_created_session(name);
        return Err(io::Error::other(summon_i18n::text!("re1fdd7bd10")));
    }
    identity.ownership_marker = ownership_marker.to_string();
    let current = inspect_handoff_identity(name, ownership_marker)?
        .ok_or_else(|| io::Error::other(summon_i18n::text!("r1840cc2b74")))?;
    if current != identity {
        return Err(io::Error::other(summon_i18n::text!("r85f2a1ea6e")));
    }
    Ok(OwnedHandoffStaging { identity })
}

/// staging 一次性 env 文件的落点:`<summon_home>/run/launch-env/<staging 名>.env`。
/// staging 名已被 `validate_staging_name` 限成 `summon-bridge-handoff-<uuid>`(纯 ASCII 字母数字 +
/// `-`),不可能穿目录;与会话资产分开放,便于一眼看出是易失的启动材料。
fn staging_env_file_path(name: &str) -> PathBuf {
    crate::runtime::paths::summon_home()
        .join("run")
        .join("launch-env")
        .join(format!("{name}.env"))
}

/// 写一次性启动 env 文件(0600)。非法键名会被跳过——staging 的键都由 `validate_env` 先筛过,
/// 这里再跳过一次只是防御,真跳到了说明上游校验漏了,故直接判负而不是静默起一个缺环境的会话。
fn write_launch_env_file(path: &Path, env: &[(String, String)]) -> io::Result<()> {
    let (body, skipped) = summon_core::launch_env::render_launch_env_file(env);
    if !skipped.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::format!("r9f4e47cc26"),
        ));
    }
    crate::runtime::fsio::atomic_write_mode(path, body.as_bytes(), 0o600)
        .map_err(|e| io::Error::other(summon_i18n::format!("r236160cb59")))
}

fn inspect_handoff_identity(
    name: &str,
    ownership_marker: &str,
) -> io::Result<Option<TmuxSessionIdentity>> {
    validate_tmux_name(name)?;
    validate_marker(ownership_marker)?;
    // 有其它 tmux session 存活时，部分版本的 display-message 对不存在的 exact target
    // 仍可能 success + 空输出；先用严格 has-session 分类，避免把“尚未 launch”误报为损坏身份。
    if !has_session_strict(name)? {
        return Ok(None);
    }
    let output = Command::new(tmux_bin())
        .args([
            "display-message",
            "-p",
            "-t",
            &format!("={}:", name),
            "-F",
            "#{session_name},#{session_id},#{session_created},#{pane_id}",
        ])
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let identity = parse_identity(&String::from_utf8_lossy(&output.stdout), ownership_marker)?;
    let marker = Command::new(tmux_bin())
        .args([
            "show-environment",
            "-t",
            &exact_session_target(name),
            HANDOFF_STAGING_OWNER_ENV,
        ])
        .stderr(Stdio::null())
        .output()?;
    if !marker.status.success()
        || String::from_utf8_lossy(&marker.stdout).trim()
            != format!("{HANDOFF_STAGING_OWNER_ENV}={ownership_marker}")
    {
        return Err(io::Error::other(summon_i18n::text!("r00f50e2b87")));
    }
    Ok(Some(identity))
}

/// fail-closed exact probe：只有 tmux 明确报告 server/session 不存在才返回 false。
pub(crate) fn has_session_strict(name: &str) -> io::Result<bool> {
    validate_tmux_name(name)?;
    probe_tmux_target(&exact_session_target(name))
}

fn probe_tmux_target(target: &str) -> io::Result<bool> {
    let output = Command::new(tmux_bin())
        .env("LC_ALL", "C")
        .args(["has-session", "-t", target])
        .stdout(Stdio::null())
        .output()?;
    classify_tmux_probe(output.status.success(), &output.stderr)
}

fn classify_tmux_probe(success: bool, stderr: &[u8]) -> io::Result<bool> {
    if success {
        return Ok(true);
    }
    let stderr = std::str::from_utf8(stderr)
        .map_err(|_| io::Error::other(summon_i18n::text!("ra09063735c")))?
        .to_ascii_lowercase();
    let detail = stderr.trim();
    let server_socket_absent = !detail.contains('\n')
        && detail.starts_with("error connecting to ")
        && (detail.ends_with("(no such file or directory)")
            || detail.ends_with("(connection refused)"));
    if detail.contains("can't find session")
        || detail.contains("no server running")
        || server_socket_absent
    {
        return Ok(false);
    }
    Err(io::Error::other(summon_i18n::format!(
        "reb075494fe",
        detail
    )))
}

fn cleanup_unmarked_created_session(name: &str) -> io::Result<()> {
    validate_staging_name(name)?;
    let status = Command::new(tmux_bin())
        .args(["kill-session", "-t", &exact_session_target(name)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(summon_i18n::text!("r5e1aa80e04")))
    }
}

fn exact_session_target(name: &str) -> String {
    format!("={name}")
}

fn parse_identity(output: &str, ownership_marker: &str) -> io::Result<TmuxSessionIdentity> {
    let fields = output.trim().split(TMUX_FORMAT_SEP).collect::<Vec<_>>();
    if fields.len() != 4
        || fields[0].is_empty()
        || !fields[1].starts_with('$')
        || fields[2].parse::<u64>().is_err()
        || !fields[3].starts_with('%')
    {
        return Err(io::Error::other(summon_i18n::text!("re0d7419fa4")));
    }
    Ok(TmuxSessionIdentity {
        name: fields[0].to_string(),
        session_id: fields[1].to_string(),
        session_created: fields[2].parse().unwrap_or_default(),
        pane_id: fields[3].to_string(),
        ownership_marker: ownership_marker.to_string(),
    })
}

fn validate_staging_name(name: &str) -> io::Result<()> {
    validate_tmux_name(name)?;
    let nonce = name.strip_prefix(HANDOFF_STAGING_PREFIX).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::text!("r68e80b46ab"),
        )
    })?;
    uuid::Uuid::parse_str(nonce).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::text!("rce1a60e57f"),
        )
    })?;
    Ok(())
}

/// tmux `-F` 格式串里的字段分隔符。
///
/// **绝不能用制表符或任何控制字符**:tmux(实测 3.7b)在展开 `-F` 时会把不可打印字符改写成
/// `_` 再输出 —— 代码里写 `\t`,`display-message -p` 吐回来的是下划线。按 `\t` 切分于是只得到
/// 一个字段,解析器判 Err,而调用方把 Err 当成「读不到」静默降级:
///
/// - `pane_cursor()` Err → 注入闸的 placeholder 探测第一步就退出 → 空输入框被一律判成「有草稿」
///   → 每条消息都被拒(owner 2026-08-15 报的「attach 上去只有 placeholder」就是这个);
/// - `parse_identity()` Err → 会话身份核对拿不到结果。
///
/// 逗号安全:`session_name` 受 `validate_tmux_name` 限制只含 `[A-Za-z0-9_-]`,其余字段是
/// `$`/`%` 前缀的数字,四个字段里都不可能出现逗号。
const TMUX_FORMAT_SEP: char = ',';

fn validate_tmux_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::text!("r73f323f5c1"),
        ));
    }
    Ok(())
}

fn validate_marker(marker: &str) -> io::Result<()> {
    if marker.len() < 32
        || marker.len() > 128
        || !marker.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::text!("rf9507a6df1"),
        ));
    }
    Ok(())
}

fn validate_env(key: &str, value: &str) -> io::Result<()> {
    if key.is_empty() || key.len() > 128 || value.len() > 16 * 1024 || value.contains('\0') || {
        let mut bytes = key.bytes();
        !bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    } {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            summon_i18n::text!("reb9a35934a"),
        ));
    }
    Ok(())
}

/// 对系统 `tmux` 的薄封装。每个实例绑定一个 session 名。
pub struct Tmux {
    pub session: String,
}

/// 当前 pane 的终端光标与 attach 状态。`cursor_x/y` 是 tmux 可见 grid 的 0-based 坐标;
/// `attached_clients` 只数有人能经它打字的客户端:非只读,且不是 summon 自己起的网页终端客户端
/// (见 [`spawn_web_client`])。没人能输入的客户端算进来,会让注入闸跳过空缓冲区探测,把 claude
/// 画在输入框里的建议句当成草稿拒掉消息。网页终端的两种页面都不计入:显示客户端的 pty 上没有
/// 任何输入来源,可操作页的按键经 [`Tmux::send_bytes`] 直送窗格,不经客户端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneCursor {
    pub x: usize,
    pub y: usize,
    pub visible: bool,
    pub attached_clients: usize,
}

/// tmux 窗口 id(`#{window_id}` 的 `@N` 形态),整个 tmux 服务上唯一且跟着窗口走。
///
/// 窗口级的读写(`window-size`、`resize-window`、窗口尺寸)一律按它寻址,不用 `={会话}:`:
/// 后者指的是**此刻**的当前窗口,而钉住要在同一个窗口上取值、写回。钉住期间当前窗口换掉
/// (窗格里的程序继承 `TMUX` 后自己 `new-window`,或本地客户端按 `C-b c`)是常态,按会话寻址的话
/// 恢复会写到另一个窗口上:原窗口永远停在 `manual`,而新窗口反倒没钉住,浏览器的 pty 尺寸开始
/// 改它。写是成功的,只是落在了别处,没有任何报错。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowId(String);

impl WindowId {
    /// 解析 `#{window_id}` 的输出。只认 `@` + 十进制数字:认不出就是读错了目标,宁可报错也不能
    /// 把一段来路不明的串当成 `-t` 目标发给 tmux。
    pub fn parse(raw: &str) -> io::Result<Self> {
        let id = raw.trim();
        let digits = id.strip_prefix('@').unwrap_or_default();
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(io::Error::other(summon_i18n::text!("rdf9c17a364")));
        }
        Ok(Self(id.to_string()))
    }

    pub fn as_target(&self) -> &str {
        &self.0
    }

    /// 窗口尺寸与状态栏行数。网页终端按「窗口高 + 状态栏」开 pty,整窗才能一屏显示完。
    pub fn geometry(&self) -> io::Result<WindowGeometry> {
        let out = Command::new(tmux_bin())
            .args([
                "display-message",
                "-p",
                "-t",
                self.as_target(),
                "-F",
                "#{window_width},#{window_height},#{status}",
            ])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(summon_i18n::text!("ref992cd6be")));
        }
        parse_window_geometry(&String::from_utf8_lossy(&out.stdout))
    }

    /// 窗口级 `window-size` 的取值;这个窗口上没单独设过时为 None(沿用全局)。
    pub fn size_option(&self) -> io::Result<Option<String>> {
        let out = Command::new(tmux_bin())
            .args([
                "show-options",
                "-wqv",
                "-t",
                self.as_target(),
                "window-size",
            ])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(summon_i18n::text!("r98ef22cec7")));
        }
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }

    /// 设窗口级 `window-size`;None = 撤掉窗口级设置,回到全局。
    pub fn set_size_option(&self, value: Option<&str>) -> io::Result<()> {
        let mut args = vec!["set-option", "-w", "-t", self.as_target()];
        match value {
            Some(v) => args.extend(["window-size", v]),
            None => {
                args[1] = "-wu";
                args.push("window-size");
            }
        }
        let status = Command::new(tmux_bin()).args(&args).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(summon_i18n::text!("r8a726ecbcd")))
        }
    }

    /// 把窗口定在给定尺寸。tmux 同时把窗口级 `window-size` 设成 `manual`,并把这个尺寸记作 manual
    /// 尺寸;单设 `window-size manual` 会把窗口弹回创建时的尺寸,被本地客户端撑大过的窗口会缩回去。
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let status = Command::new(tmux_bin())
            .args([
                "resize-window",
                "-t",
                self.as_target(),
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
            ])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(summon_i18n::text!("re2cea2c3ab")))
        }
    }
}

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 窗口(不是 pane)的尺寸与状态栏占用行数。网页终端按整窗尺寸开 pty(而非某个 pane 的尺寸),
/// 否则别的 attach 客户端会把窗口压缩到比这个 pane 还小。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    pub cols: u16,
    pub rows: u16,
    pub status_lines: u16,
}

/// tmux 窗格 id(`#{pane_id}` 的 `%N` 形态),整个 tmux 服务上唯一且跟着窗格走。
///
/// 网页滚动先查窗格状态、再往里送事件,两步都按它寻址:按 `={会话}:` 寻址指的是**此刻**的当前
/// 窗格,两步之间当前窗口或窗格一换,查的和送的就不是同一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneId(String);

/// 送滚轮事件那一刻由 tmux 自己再判一次的条件,与 [`PaneMouse::takes_wheel`] 同一条判据:没开
/// `synchronize-panes`、不在任何模式里、开着 SGR 汇报、至少开着一种跟踪,展开为 `1`,否则为 `0`。
///
/// 查询与发送是两次 tmux 调用,中间窗格可能进了 copy-mode(本地有人刚开始滚屏)、程序关了鼠标
/// 汇报、或窗口开了同步输入:第一种会被序列开头的 ESC 取消掉,后两种会把整段序列当成输入框里的字
/// (同步输入时是隔壁窗格的输入框)。闸放进送的那条 tmux 命令里判:tmux 服务单线程,`if-shell -F`
/// 判完就在同一轮命令队列里接着跑 `send-keys`,中间插不进别的客户端的按键。
const WHEEL_GATE: &str = "#{?synchronize-panes,0,#{?pane_in_mode,0,#{?mouse_sgr_flag,\
#{?mouse_any_flag,1,#{?mouse_button_flag,1,#{?mouse_standard_flag,1,0}}},0}}}";

impl PaneId {
    /// 解析 `#{pane_id}` 的输出。只认 `%` + 十进制数字:它要拼进 tmux 解析的命令串里(见
    /// [`Self::send_wheel_argv`]),认不出就报错,不把来路不明的串交给 tmux 的命令解析器。
    pub fn parse(raw: &str) -> io::Result<Self> {
        let id = raw.trim();
        let digits = id.strip_prefix('%').unwrap_or_default();
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(io::Error::other(summon_i18n::text!("r836db42fea")));
        }
        Ok(Self(id.to_string()))
    }

    pub fn as_target(&self) -> &str {
        &self.0
    }

    /// 送滚轮事件的 argv:`if-shell -F` 在 tmux 里按 [`WHEEL_GATE`] 判,成立才跑里面那条
    /// `send-keys -H`,不成立就跑 `display-message -p` 把 [`WHEEL_REFUSED`] 打到标准输出——闸不过与
    /// 送出去了在退出码上一样,只有这样才分得开。里面的命令串经 tmux 的命令解析器,只由固定的词、
    /// 校验过的窗格 id 与十六进制字节拼成。**不带 `copy-mode -q`**:窗格在模式里就不送,本地用户的
    /// copy-mode 是他的。`without_client` 给里面的 `send-keys` 加 `-c ''`,用法同 [`run_send_keys`]。
    pub(crate) fn send_wheel_argv(
        &self,
        events: &WheelEvents,
        without_client: bool,
    ) -> Vec<String> {
        let mut send = String::from("send-keys");
        if without_client {
            send.push_str(" -c ''");
        }
        send.push_str(" -t ");
        send.push_str(self.as_target());
        send.push_str(" -H");
        for b in events.as_bytes() {
            send.push_str(&format!(" {b:02x}"));
        }
        let refused = format!("display-message -p {WHEEL_REFUSED}");
        owned(&[
            "if-shell",
            "-F",
            "-t",
            self.as_target(),
            WHEEL_GATE,
            &send,
            &refused,
        ])
    }

    /// 往这个窗格送网页的滚轮事件。闸在 tmux 里判(见 [`WHEEL_GATE`]),不成立时什么都不送,返回
    /// [`WheelSend::Refused`]。被只读客户端拒时带 `-c ''` 重跑一次(见 [`run_send_keys`])。tmux 的
    /// 报错不进日志:滚动失败只是这一下没滚动,每秒可能有几十次。不排队,网页走 [`Tmux::send_wheel`]。
    fn send_wheel(&self, events: &WheelEvents) -> io::Result<WheelSend> {
        let mut out = Command::new(tmux_bin())
            .args(self.send_wheel_argv(events, false))
            .stderr(Stdio::piped())
            .output()?;
        if !out.status.success() && refused_for_read_only_client(&out.stderr) {
            out = Command::new(tmux_bin())
                .args(self.send_wheel_argv(events, true))
                .stderr(Stdio::null())
                .output()?;
        }
        if out.status.success() {
            Ok(wheel_send_outcome(&out.stdout))
        } else {
            Err(io::Error::other(summon_i18n::text!("r1d97298fb5")))
        }
    }

    /// 这个窗格的模式与鼠标汇报状态、尺寸(同 [`Tmux::pane_mouse`]);窗格已经不在就 Err。
    pub fn mouse(&self) -> io::Result<PaneMouse> {
        let (id, mouse) = query_pane_mouse(self.as_target())?;
        if id != *self {
            return Err(io::Error::other(summon_i18n::text!("rc89efecd02")));
        }
        Ok(mouse)
    }

    /// 抓这个窗格此刻的可见画面(不带历史)。与 [`Tmux::capture_visible`] 不同,抓不到就 Err,
    /// 不拿空串冒充「屏上什么都没有」。
    pub fn capture_visible(&self) -> io::Result<String> {
        let out = Command::new(tmux_bin())
            .args(["capture-pane", "-p", "-t", self.as_target()])
            .stderr(Stdio::null())
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(summon_i18n::text!("r7ef61479b2")));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// tmux 那边的闸不成立时,`if-shell` 的另一支打到标准输出的记号。
const WHEEL_REFUSED: &str = "summon-wheel-refused";

/// 一次送滚轮的结果(tmux 本身出错另走 Err)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelSend {
    /// 闸成立,`send-keys` 跑了。
    Sent,
    /// 闸不成立,什么都没送。
    Refused,
}

/// 按 `if-shell` 的标准输出判结果:送出去的那一支什么都不打。打了任何东西(闸不过的记号,或认不出的
/// 输出)都不算送出去——算错成送出去,断开时会翻回去这些从没翻上去的格数。
fn wheel_send_outcome(stdout: &[u8]) -> WheelSend {
    if String::from_utf8_lossy(stdout).trim().is_empty() {
        WheelSend::Sent
    } else {
        WheelSend::Refused
    }
}

impl Tmux {
    pub fn new(session: impl Into<String>) -> Self {
        Tmux {
            session: session.into(),
        }
    }

    /// tmux `-t` 目标串:`={name}:` 两段缺一不可。
    /// - `=` 前缀强制**精确匹配 session 名**:否则 tmux `-t` 默认走前缀/fnmatch,裸名 `soul-x`
    ///   会误命中带后缀的 per-chat 会话 `soul-x-g<tag>`(裸名是其前缀),致 owner 私聊与群会话
    ///   坍缩、上下文串台。
    /// - 末尾 `:` 不可省:`capture-pane`/`send-keys` 的 `-t` 是 **target-pane**,裸 `=name`
    ///   解析不出 pane(报 "can't find pane: =name",capture 静默返回空 → 抓不到屏/发不出键)。
    ///   `=name:` 让它成为「session 精确匹配 + 当前窗口活动 pane」的合法目标,且对 session 类
    ///   命令(has/kill)同样精确。两段都只是 tmux 目标语法,经 argv 直传不过 shell,无展开风险。
    fn target(&self) -> String {
        format!("={}:", self.session)
    }

    pub fn has_session(&self) -> bool {
        // 探测命令:吞掉 "no server running"/"can't find session" 这类预期 stderr,别污染日志
        Command::new(tmux_bin())
            .args(["has-session", "-t", &self.target()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// 读取活动 pane 的真实工作目录。只查询 tmux，不向 pane 注入 `pwd`，因此不会唤醒或污染
    /// provider 会话。调用前可先用 [`Self::has_session`] 区分“未运行”和“读取失败”。
    pub fn pane_current_path(&self) -> io::Result<String> {
        let output = Command::new(tmux_bin())
            .args([
                "display-message",
                "-p",
                "-t",
                &self.target(),
                "-F",
                "#{pane_current_path}",
            ])
            .stderr(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(summon_i18n::text!("r7d9b45c9ec")));
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            return Err(io::Error::other(summon_i18n::text!("reb64621443")));
        }
        Ok(path)
    }

    /// `tmux new-session` 的完整 argv(纯件,便于单测断言「凭据不在 argv 里」)。
    ///
    /// **这条 argv 会成为 tmux server 进程的命令行并长期留在 `ps` 里**(macOS 上 tmux 改不掉
    /// 自己的 argv,server 又 ppid=1 常驻),所以 `cmd` 里绝不许内联任何凭据值——环境一律走
    /// `summon_core::launch_env` 的 env 文件,这里只该出现文件路径。
    pub(crate) fn new_session_argv(&self, cwd: &str, cmd: &str) -> Vec<String> {
        ["new-session", "-d", "-s", &self.session, "-c", cwd, cmd]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// 在 cwd 后台起一个 session 跑 cmd。
    pub fn new_session(&self, cwd: &str, cmd: &str) -> io::Result<()> {
        let status = Command::new(tmux_bin())
            .args(self.new_session_argv(cwd, cmd))
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("tmux new-session failed"))
        }
    }

    /// 送一行文本(字面量),再单独送 Enter,避免 tmux 把文本里的键名当快捷键。
    pub fn send_line(&self, text: &str) -> io::Result<()> {
        self.send_text(text)?;
        self.send_enter()?;
        Ok(())
    }

    /// 送文本字面量但不提交。多 CLI driver 可组合特殊键实现自己的输入策略。
    /// `--` 终止 tmux 选项解析:否则 text 以 `-` 开头(如逐行发送的 Markdown 列表项 `- xxx`)
    /// 会被当 flag,报 `send-keys: invalid flag -`、整行被吞。`-l` 已保证字面发送。
    pub fn send_text(&self, text: &str) -> io::Result<()> {
        run_send_keys(&owned(&[
            "send-keys",
            "-t",
            &self.target(),
            "-l",
            "--",
            text,
        ]))?;
        Ok(())
    }

    /// 本会话专属的 tmux buffer 名。每个 chat 一个 tmux 会话,按会话名分桶,paste 时不会与
    /// 别的 chat 线程抢同一个 buffer。
    fn paste_buffer_name(&self) -> String {
        format!("summon-{}", self.session)
    }

    pub(crate) fn load_buffer_argv(&self) -> Vec<String> {
        ["load-buffer", "-b", &self.paste_buffer_name(), "-"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// `-p`:带 bracketed paste 标记(仅当 pane 里的程序自己请求了粘贴模式,tmux 才真的加,
    /// 没请求就退化成普通写入)。`-d`:贴完删掉 buffer,不在 tmux 里堆积会话正文。
    pub(crate) fn paste_buffer_argv(&self) -> Vec<String> {
        [
            "paste-buffer",
            "-b",
            &self.paste_buffer_name(),
            "-t",
            &self.target(),
            "-p",
            "-d",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// 以**粘贴**语义把整段文本送进 pane(不提交)。正文经 stdin 进 tmux buffer,不走 argv ——
    /// 一条 prompt 可以有几十 KB,argv 有上限而管道没有。
    pub fn paste_text(&self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let mut child = Command::new(tmux_bin())
            .args(self.load_buffer_argv())
            .stdin(Stdio::piped())
            .spawn()?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other(summon_i18n::text!("r6323c71d20")))?;
            stdin.write_all(text.as_bytes())?;
        }
        if !child.wait()?.success() {
            return Err(io::Error::other("tmux load-buffer failed"));
        }
        let status = Command::new(tmux_bin())
            .args(self.paste_buffer_argv())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("tmux paste-buffer failed"));
        }
        Ok(())
    }

    /// 只送一个回车(用于接受信任目录提示等)。
    pub fn send_enter(&self) -> io::Result<()> {
        run_send_keys(&owned(&["send-keys", "-t", &self.target(), "Enter"]))?;
        Ok(())
    }

    /// 发送一个 tmux key 名(如 `a` / `C-u`),用于处理 TUI 选择。
    pub fn send_key(&self, key: &str) -> io::Result<()> {
        run_send_keys(&owned(&["send-keys", "-t", &self.target(), key]))?;
        Ok(())
    }

    /// `send_bytes` 的逐次 argv(纯件,便于单测)。每块至多 [`SEND_BYTES_CHUNK`] 字节,离 ARG_MAX
    /// 很远;切点不落在 UTF-8 字符或转义序列中间(见 `send_chunk_ends`)。每次先 `copy-mode -q`:窗格在任何模式里(本地有人滚屏进了 copy-mode)时,`send-keys`
    /// 送进去的字节被模式吞掉且照样返回成功——网页这边没有按键表,自己退不出来。
    pub(crate) fn send_bytes_argvs(&self, bytes: &[u8]) -> Vec<Vec<String>> {
        let target = self.target();
        let mut start = 0;
        send_chunk_ends(bytes, SEND_BYTES_CHUNK)
            .into_iter()
            .map(|end| {
                let chunk = &bytes[start..end];
                start = end;
                let mut argv = owned(&[
                    "copy-mode",
                    "-q",
                    "-t",
                    &target,
                    ";",
                    "send-keys",
                    "-t",
                    &target,
                    "-H",
                ]);
                argv.extend(chunk.iter().map(|b| format!("{b:02x}")));
                argv
            })
            .collect()
    }

    /// 把字节原样送进本会话当前窗格(网页终端可操作页的输入)。`-H` 让 tmux 把每个 00–ff 当作
    /// 字面字节写给窗格里的程序,不经任何客户端的按键表:tmux 客户端这一层的前缀键、命令提示符、
    /// 切会话都够不着,Ctrl-B、Esc、方向键与多字节 UTF-8 逐字节到达。窗格里的程序拿到这些字节后
    /// 照常执行,这里不设防。按序逐块执行;任一块失败即报错。
    ///
    /// 整段在本会话的网页输入锁里送完(见 [`input_lock`])。
    pub fn send_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let lock = input_lock(&self.session);
        let _serial = lock.lock().unwrap_or_else(PoisonError::into_inner);
        for argv in self.send_bytes_argvs(bytes) {
            if !run_send_keys(&argv)?.success() {
                return Err(io::Error::other(summon_i18n::text!("rabe6219800")));
            }
        }
        Ok(())
    }

    /// 往本会话里查到的那个窗格送网页的滚轮事件(见 `PaneId` 的同名方法),在本会话的网页输入锁里
    /// 送(见 [`input_lock`])。
    pub fn send_wheel(&self, pane: &PaneId, events: &WheelEvents) -> io::Result<WheelSend> {
        let lock = input_lock(&self.session);
        let _serial = lock.lock().unwrap_or_else(PoisonError::into_inner);
        pane.send_wheel(events)
    }

    /// 清空当前输入行。Cursor Agent 的交互 TUI 有时会把上一轮输入留在编辑区,
    /// 发送新 prompt 前先 C-u,避免两轮文本拼接。
    pub fn clear_input(&self) -> io::Result<()> {
        self.send_key("C-u")?;
        Ok(())
    }

    /// 确保 pane **不在 copy-mode**。用户滚屏 / 鼠标选择会让 pane 进入 copy-mode,此时普通
    /// `send-keys` 的按键被 copy-mode 当导航吃掉、**不进** CLI TUI —— 注入的 prompt 静默落空,
    /// 本轮空等到 STUCK_IDLE 超时(现象:点会话面板的退出/发消息「对 agent 没反应」、总结失败)。
    /// `copy-mode -q` 幂等:在模式里就退出、不在则 no-op(不会反而进模式),故每次发键前无脑调一次。
    /// best-effort:失败也继续发送(退化回原行为),stderr 吞掉别污染日志。
    pub fn exit_copy_mode(&self) {
        let _ = Command::new(tmux_bin())
            .args(["copy-mode", "-q", "-t", &self.target()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// 抓 pane 文本,含最近 500 行 scrollback(-S):长回复时助手标记 `⏺` 会滚出可见区,
    /// 必须带 scrollback 才能被 extract_answer 锚定。无 -e:tmux 已去转义。
    pub fn capture(&self) -> io::Result<String> {
        let out = Command::new(tmux_bin())
            .args(["capture-pane", "-t", &self.target(), "-p", "-S", "-500"])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 抓**可见** pane 并保留 SGR 转义(`-e`)。只给「输入行上那段字是不是 claude 自己画的」
    /// 这一个判据用(见 `pane_input::prompt_text_is_dim`)——它是纯文本抓屏里唯一分不出来的东西。
    ///
    /// 不带 scrollback:判据只看底部那一行输入行,拉 500 行历史纯属浪费。
    /// **不拿它替代 [`Self::capture`]**:转义序列会让「两拍逐字节相等」这类稳定性判据
    /// 变得脆(光标位置、重绘策略都会改字节),那些判据要的正是 tmux 已经去过转义的文本。
    pub fn capture_styled(&self) -> io::Result<String> {
        let out = Command::new(tmux_bin())
            .args(["capture-pane", "-t", &self.target(), "-p", "-e"])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 抓**可见** pane（不带 scrollback），给会话状态快照使用：只读、轻量。
    pub fn capture_visible(&self) -> io::Result<String> {
        let out = Command::new(tmux_bin())
            .args(["capture-pane", "-t", &self.target(), "-p"])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 读取当前 pane 光标、可见性与可写客户端数。光标三项同一个 display-message 快照;客户端数
    /// 另查一次——两次之间有人 attach 属于探测本就处理的竞争(探测后会再读一次)。解析失败一律
    /// Err,让输入闸 fail-closed。
    pub fn pane_cursor(&self) -> io::Result<PaneCursor> {
        let out = Command::new(tmux_bin())
            .args([
                "display-message",
                "-p",
                "-t",
                &self.target(),
                "-F",
                "#{cursor_x},#{cursor_y},#{cursor_flag}",
            ])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(summon_i18n::text!("rdaaa0adbed")));
        }
        let mut cursor = parse_pane_cursor(&String::from_utf8_lossy(&out.stdout))?;
        cursor.attached_clients = self.writable_clients()?;
        Ok(cursor)
    }

    /// attach 在这个会话上、有人能经它打字的客户端数:非只读,且不是 summon 的网页终端客户端。
    /// 两类都不该被注入闸当成「有人在键盘前」——见 [`PaneCursor`] 文档。
    pub fn writable_clients(&self) -> io::Result<usize> {
        let out = Command::new(tmux_bin())
            .args([
                "list-clients",
                "-t",
                &exact_session_target(&self.session),
                "-F",
                "#{client_readonly},#{client_pid}",
            ])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(summon_i18n::text!("rdbb21085f0")));
        }
        // 先列客户端、后取登记表:列表里出现的网页客户端此刻一定已经登记(见 `spawn_web_client`)。
        parse_writable_clients(&String::from_utf8_lossy(&out.stdout), &web_clients())
    }

    /// 会话此刻的当前窗口 id。窗口级操作都按它寻址:见 [`WindowId`]。
    pub fn current_window(&self) -> io::Result<WindowId> {
        let out = Command::new(tmux_bin())
            .args([
                "display-message",
                "-p",
                "-t",
                &self.target(),
                "-F",
                "#{window_id}",
            ])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(summon_i18n::text!("rd80075c888")));
        }
        WindowId::parse(&String::from_utf8_lossy(&out.stdout))
    }

    /// 当前窗格的 id、模式与鼠标汇报状态、尺寸,一次查询读出。网页滚动据此决定送不送、送到哪一格;
    /// 读不出或认不出一律 Err,滚动随之丢掉。
    pub fn pane_mouse(&self) -> io::Result<(PaneId, PaneMouse)> {
        query_pane_mouse(&self.target())
    }

    /// 最后一个客户端断开时 tmux 会不会销毁这个会话。看 `destroy-unattached` 的生效值(`-A`:会话上
    /// 没设就是继承来的全局值)。
    pub fn destroyed_on_last_detach(&self) -> io::Result<bool> {
        let out = Command::new(tmux_bin())
            .args([
                "show-options",
                "-Av",
                "-t",
                &self.target(),
                "destroy-unattached",
            ])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(summon_i18n::text!("r46018b4f80")));
        }
        Ok(last_detach_destroys(&String::from_utf8_lossy(&out.stdout)))
    }

    /// 会话被销毁时,挂在上面的客户端会不会跟着退出。看 `detach-on-destroy` 的生效值:只有 `on`
    /// 是退出;`off` / `previous` / `next` / `no-detached` 都会把客户端切到**别的**会话上。
    pub fn detaches_on_destroy(&self) -> io::Result<bool> {
        let out = Command::new(tmux_bin())
            .args([
                "show-options",
                "-Av",
                "-t",
                &self.target(),
                "detach-on-destroy",
            ])
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(summon_i18n::text!("rc132354aa1")));
        }
        Ok(detach_on_destroy_exits(&String::from_utf8_lossy(
            &out.stdout,
        )))
    }

    /// attach 客户端命令(stdio 由调用方接到 pty)。`read_only` 加 `-f read-only`,与本地
    /// `tmux attach -r` 同类。网页终端的显示客户端不标只读:tmux 3.7 起,只读客户端会让同一 tmux
    /// 服务上别的程序不带 `-c` 的 `send-keys` 被拒(见 `run_send_keys`);它不收输入靠的是 pty 主端
    /// 没有写入方。`-u` 让客户端按 UTF-8 画屏,不看区域变量:launchd 起的 daemon 常常一个都没有。
    pub fn attach_command(&self, read_only: bool) -> Command {
        let mut cmd = Command::new(tmux_bin());
        cmd.args(["-u", "attach-session"]);
        if read_only {
            cmd.args(["-f", "read-only"]);
        }
        cmd.args(["-t", &exact_session_target(&self.session)]);
        cmd
    }

    /// 销毁本 tmux 会话(`tmux kill-session`)。**全仓唯一执行 kill-session 的地方**——所有合法
    /// 销毁途径都经此封装,方便 `tmux_kill_session_call_sites_are_whitelisted` 结构守护按调用点白名单
    /// 兜底 CLAUDE.md 第一红线(回收/空闲扫描线程从不擅杀)。命名带 `_session` 以区别 `ChildKiller::kill`
    /// (无头子进程组 SIGKILL),避免二者在源码扫描里混淆。
    pub fn kill_session(&self) -> io::Result<()> {
        Command::new(tmux_bin())
            .args(["kill-session", "-t", &self.target()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(())
    }
}

/// 扫描键盘输入时所处的位置:在不在一个还没收完的 UTF-8 字符或转义序列里。
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputUnit {
    Ground,
    /// UTF-8 多字节字符,还差这么多个续字节。
    Utf8(u8),
    /// 刚读到 ESC。
    Esc,
    /// `ESC` 后跟中间字节(0x20–0x2f),等终结字节。
    EscIntermediate,
    /// `ESC O` / `ESC N`,再收一个字节就完。
    OneMore,
    /// `ESC [`,等 0x40–0x7e 的终结字节。
    Csi,
    /// OSC / DCS / SOS / PM / APC 的正文;`bel` 表示 BEL 也能收尾(OSC)。
    Text {
        bel: bool,
    },
    /// 正文里读到 ESC,下一个是 `\` 就收尾。
    TextEsc {
        bel: bool,
    },
}

impl InputUnit {
    /// 读进一个字节后的位置,以及这个字节是不是开了一个新单位。认不出的组合一律当作收完,
    /// 扫描不会卡在半截里。
    fn next(self, b: u8) -> (Self, bool) {
        let state = match self {
            Self::Ground => {
                let started = match b {
                    0x1b => Self::Esc,
                    0xc0..=0xdf => Self::Utf8(1),
                    0xe0..=0xef => Self::Utf8(2),
                    0xf0..=0xf7 => Self::Utf8(3),
                    _ => Self::Ground,
                };
                return (started, started != Self::Ground);
            }
            Self::Utf8(n) if (0x80..=0xbf).contains(&b) => {
                if n > 1 {
                    Self::Utf8(n - 1)
                } else {
                    Self::Ground
                }
            }
            // 残缺的字符就此作罢,这个字节从头算。
            Self::Utf8(_) => return Self::Ground.next(b),
            // `ESC ESC …`(Alt+Esc、Alt+方向键)算同一个单位。
            Self::Esc => match b {
                0x1b => Self::Esc,
                b'[' => Self::Csi,
                b']' => Self::Text { bel: true },
                b'P' | b'X' | b'^' | b'_' => Self::Text { bel: false },
                b'O' | b'N' => Self::OneMore,
                0x20..=0x2f => Self::EscIntermediate,
                _ => Self::Ground,
            },
            Self::EscIntermediate if (0x20..=0x2f).contains(&b) => Self::EscIntermediate,
            Self::EscIntermediate | Self::OneMore => Self::Ground,
            Self::Csi if (0x20..=0x3f).contains(&b) => Self::Csi,
            Self::Csi => Self::Ground,
            Self::Text { bel } => match b {
                0x07 if bel => Self::Ground,
                0x1b => Self::TextEsc { bel },
                _ => Self::Text { bel },
            },
            Self::TextEsc { .. } if b == b'\\' => Self::Ground,
            Self::TextEsc { bel } => Self::Text { bel },
        };
        (state, false)
    }
}

/// `send_bytes` 各块的结束位置。每块至多 `max` 字节,切点退到没收完的 UTF-8 字符或转义序列之前:
/// 两次 `send-keys` 之间窗格里的程序可能先读走前半截,劈开的 `ESC [ 2 0 1 ~`(粘贴结束标记)会被
/// claude 当成一个单独的 Esc。一个单位比整块还长时只能按块切,保证每块都有进展。
fn send_chunk_ends(bytes: &[u8], max: usize) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut chunk_start = 0;
    let mut unit = InputUnit::Ground;
    let mut unit_start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if i - chunk_start == max {
            let cut = if unit != InputUnit::Ground && unit_start > chunk_start {
                unit_start
            } else {
                i
            };
            ends.push(cut);
            chunk_start = cut;
        }
        let (next, starts) = unit.next(b);
        if starts {
            unit_start = i;
        }
        unit = next;
    }
    if chunk_start < bytes.len() {
        ends.push(bytes.len());
    }
    ends
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// 跑一条含 `send-keys` 的 tmux 命令。
///
/// 命令行上不带 `-c` 时,`send-keys` 的目标客户端是整个 tmux 服务上「最近活跃会话里最近活跃的
/// 客户端」,与 `-t` 指的会话无关;tmux 3.7 起这个客户端是只读的就整条拒绝(`client is
/// read-only`),一个键都不送。网页终端的显示客户端不标只读,不会招来这条拒绝;但用户自己的
/// `tmux attach -r` 挂着且是最近活跃的那个时,任何会话的 `send-keys`——summon 的注入与网页输入
/// 也在内——照样被拒,所以这里留着重跑:带上 `-c ""`,空名字不匹配任何客户端,`send-keys` 于是
/// 没有目标客户端,与没人挂着时一样把键送进窗格。只在被拒时才加:3.4 之前的 `send-keys` 不认
/// `-c`,而会这样拒绝的版本都认。
fn run_send_keys(argv: &[String]) -> io::Result<std::process::ExitStatus> {
    let out = Command::new(tmux_bin())
        .args(argv)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if out.status.success() || !refused_for_read_only_client(&out.stderr) {
        // tmux 的报错照旧进 daemon 的 stderr;写不出去也不影响发键。
        let _ = io::stderr().write_all(&out.stderr);
        return Ok(out.status);
    }
    Command::new(tmux_bin())
        .args(without_target_client(argv))
        .stdout(Stdio::null())
        .status()
}

fn refused_for_read_only_client(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains("client is read-only")
}

/// 在处于命令位置(argv 开头或 `;` 之后)的每个 `send-keys` 后面插 `-c ""`。
fn without_target_client(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len() + 2);
    let mut command_start = true;
    for arg in argv {
        out.push(arg.clone());
        if command_start && arg == "send-keys" {
            out.extend(["-c".to_string(), String::new()]);
        }
        command_start = arg == ";";
    }
    out
}

fn parse_pane_cursor(raw: &str) -> io::Result<PaneCursor> {
    let mut fields = raw.trim().split(TMUX_FORMAT_SEP);
    let parse = |s: Option<&str>, name: &str| -> io::Result<usize> {
        s.ok_or_else(|| io::Error::other(summon_i18n::format!("rc4a81ddb4a")))?
            .parse::<usize>()
            .map_err(|_| io::Error::other(summon_i18n::format!("rf2621edb8e")))
    };
    let x = parse(fields.next(), "cursor_x")?;
    let y = parse(fields.next(), "cursor_y")?;
    let visible = match parse(fields.next(), "cursor_flag")? {
        0 => false,
        1 => true,
        _ => return Err(io::Error::other(summon_i18n::text!("rc091932e87"))),
    };
    if fields.next().is_some() {
        return Err(io::Error::other(summon_i18n::text!("raa94170e74")));
    }
    // attached_clients 不在这三个字段里,由调用方(pane_cursor)另查 writable_clients 回填。
    Ok(PaneCursor {
        x,
        y,
        visible,
        attached_clients: 0,
    })
}

/// `list-clients -F '#{client_readonly},#{client_pid}'` 的解析,每行一个客户端:`0`=可写、
/// `1`=只读,后面是客户端进程号。只数可写且进程号不在 `web_clients` 里的——只读客户端打不了字,
/// summon 的网页终端客户端的 pty 上没有输入来源,都不该算「有人在键盘前」。任一字段认不出就
/// Err,让注入闸 fail-closed。
fn parse_writable_clients(raw: &str, web_clients: &BTreeSet<u32>) -> io::Result<usize> {
    let mut writable = 0usize;
    for line in raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let bad = || io::Error::other(summon_i18n::format!("rc381cf992a"));
        let (readonly, pid) = line.split_once(TMUX_FORMAT_SEP).ok_or_else(bad)?;
        let pid: u32 = pid.parse().map_err(|_| bad())?;
        match readonly {
            "0" if !web_clients.contains(&pid) => writable += 1,
            "0" | "1" => {}
            _ => return Err(bad()),
        }
    }
    Ok(writable)
}

/// summon 为网页终端起的 tmux 客户端进程号(`#{client_pid}`:客户端就是 summon 直接起的 tmux
/// 进程)。显示客户端不标只读,注入闸靠这张表认出它们。
static WEB_CLIENTS: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

fn web_clients() -> MutexGuard<'static, BTreeSet<u32>> {
    // 表里只有进程号,持锁的一方 panic 也不会留下半改的状态。
    WEB_CLIENTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 一个登记在册的网页终端客户端。析构即注销,客户端怎么结束(正常断开、出错返回、panic 展开)
/// 都不会把进程号留在表里。持有者在回收客户端进程(wait)之后再放掉它:注销早了,还挂在 tmux
/// 上的客户端会被算成有人在键盘前。
pub struct WebClientRegistration {
    pid: u32,
}

impl Drop for WebClientRegistration {
    fn drop(&mut self) {
        web_clients().remove(&self.pid);
    }
}

/// 仅测试:进程号在不在登记表里。
#[cfg(test)]
pub(crate) fn web_client_registered(pid: u32) -> bool {
    web_clients().contains(&pid)
}

/// 起一个网页终端的 tmux 客户端并登记它的进程号。spawn 与登记在同一把锁里,而
/// [`Tmux::writable_clients`] 先列客户端、后取表:客户端能出现在列表里时 spawn 已经开始,取表
/// 就得等登记做完,中间没有它被当成「有人在键盘前」的空当。
pub fn spawn_web_client(cmd: &mut Command) -> io::Result<(Child, WebClientRegistration)> {
    let mut clients = web_clients();
    let child = cmd.spawn()?;
    let pid = child.id();
    clients.insert(pid);
    Ok((child, WebClientRegistration { pid }))
}

/// 按 `target`(`={会话}:` 或 `%N`)查窗格的 id、模式与鼠标汇报状态、尺寸。
fn query_pane_mouse(target: &str) -> io::Result<(PaneId, PaneMouse)> {
    let out = Command::new(tmux_bin())
        .args([
            "display-message",
            "-p",
            "-t",
            target,
            "-F",
            "#{pane_id},#{pane_in_mode},#{mouse_sgr_flag},#{mouse_any_flag},\
#{mouse_button_flag},#{mouse_standard_flag},#{synchronize-panes},#{pane_width},#{pane_height}",
        ])
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(summon_i18n::text!("r02e304f0a8")));
    }
    parse_pane_mouse(&String::from_utf8_lossy(&out.stdout))
}

/// [`Tmux::pane_mouse`] 那条格式的解析:窗格 id、六个 0/1 标志、宽、高,一个都不能多、不能少。
fn parse_pane_mouse(raw: &str) -> io::Result<(PaneId, PaneMouse)> {
    let bad = || io::Error::other(summon_i18n::format!("r4215bb4565"));
    let fields: Vec<&str> = raw.trim().split(TMUX_FORMAT_SEP).collect();
    let [id, in_mode, sgr, any, button, standard, synchronized, width, height] = fields[..] else {
        return Err(bad());
    };
    let flag = |f: &str| match f {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(bad()),
    };
    let size = |f: &str| f.parse::<u16>().map_err(|_| bad());
    Ok((
        PaneId::parse(id)?,
        PaneMouse {
            in_mode: flag(in_mode)?,
            sgr: flag(sgr)?,
            any: flag(any)?,
            button: flag(button)?,
            standard: flag(standard)?,
            synchronized: flag(synchronized)?,
            width: size(width)?,
            height: size(height)?,
        },
    ))
}

/// 网页往同一个会话送的按键与滚轮一个接一个来。可操作页一段长输入要分几次 `send-keys` 送,别的
/// 连接的滚轮事件夹进两块之间,就落在粘贴标记的中间、成了粘贴内容的一部分。按会话名一把锁,只在
/// 这些 tmux 调用期间持有。
static INPUT_LOCKS: Mutex<std::collections::BTreeMap<String, Arc<Mutex<()>>>> =
    Mutex::new(std::collections::BTreeMap::new());

/// 这个会话的网页输入锁。同名拿到的是同一把。没人拿着的锁在这里顺手清掉,表不随会话名一直长。
fn input_lock(session: &str) -> Arc<Mutex<()>> {
    // 表里只有锁,持表锁的一方 panic 也不会留下半改的状态。
    let mut locks = INPUT_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    locks.retain(|name, lock| name == session || Arc::strong_count(lock) > 1);
    Arc::clone(locks.entry(session.to_string()).or_default())
}

/// `#{window_width},#{window_height},#{status}` 的解析。`status` 通常是 `on`/`off`(单行状态栏
/// 开或关);tmux 也支持 `status 2` 这类多行状态栏,此时该字段直接是行数。
fn parse_window_geometry(raw: &str) -> io::Result<WindowGeometry> {
    let bad = || io::Error::other(summon_i18n::format!("re4283c389f"));
    let mut fields = raw.trim().split(TMUX_FORMAT_SEP);
    let cols: u16 = fields.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let rows: u16 = fields.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let status = fields.next().ok_or_else(bad)?;
    let status_lines: u16 = match status {
        "off" => 0,
        "on" => 1,
        n => n.parse().map_err(|_| bad())?,
    };
    if fields.next().is_some() {
        return Err(bad());
    }
    Ok(WindowGeometry {
        cols,
        rows,
        status_lines,
    })
}

/// `destroy-unattached` 生效值的判定:只有 `off` 算断开后会话还在。`on` / `keep-last` /
/// `keep-group` 与认不出的取值一律按会销毁处理——判错的代价是拆掉一个活会话,宁可不接。
fn last_detach_destroys(effective: &str) -> bool {
    effective.trim() != "off"
}

/// `detach-on-destroy` 生效值的判定:只有 `on` 让客户端随会话一起退出。其余取值与认不出的取值
/// 都会把客户端留在服务上、切去另一个会话。
fn detach_on_destroy_exits(effective: &str) -> bool {
    effective.trim() == "on"
}

/// 客户端进程 `pid` 此刻挂在哪个会话上;列表里没有它时为 None。看的是整个 tmux 服务上的客户端。
pub fn client_session(pid: u32) -> io::Result<Option<String>> {
    let out = Command::new(tmux_bin())
        .args(["list-clients", "-F", "#{client_pid},#{session_name}"])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(summon_i18n::text!("rdbb21085f0")));
    }
    Ok(parse_client_session(
        &String::from_utf8_lossy(&out.stdout),
        pid,
    ))
}

/// `list-clients -F '#{client_pid},#{session_name}'` 里找 `pid` 那一行的会话名。会话名不含分隔符
/// (见 `validate_tmux_name`),按第一个分隔符切开即可。
fn parse_client_session(raw: &str, pid: u32) -> Option<String> {
    let pid = pid.to_string();
    raw.lines().find_map(|line| {
        let (p, session) = line.trim().split_once(TMUX_FORMAT_SEP)?;
        (p == pid).then(|| session.to_string())
    })
}

/// 列出**全部** tmux 会话名(无 server 时空)。
///
/// 要筛多个 agent 时用它 + [`filter_names`],**别对每个 agent 各调一次**
/// [`list_sessions_with_prefix`]:`tmux list-sessions` 每次都返回全量,前缀过滤是本地的事,
/// 每个 agent 各 spawn 一次 tmux 是纯浪费(5 个 agent ≈ 32ms,全堵在 Menu Bar 弹出之前)。
pub fn list_all_sessions() -> Vec<String> {
    let out = Command::new(tmux_bin())
        .args(["list-sessions", "-F", "#{session_name}"])
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|s| s.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// 从已取到的全量会话名里筛前缀(纯件,便于单测)。
pub fn filter_names(all: &[String], prefix: &str) -> Vec<String> {
    all.iter()
        .filter(|s| s.starts_with(prefix))
        .cloned()
        .collect()
}

/// 列出所有以 prefix 开头的 tmux 会话名(无 server 时空)。单个 agent 用。
pub fn list_sessions_with_prefix(prefix: &str) -> Vec<String> {
    filter_names(&list_all_sessions(), prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AttachedClient;
    use std::thread::sleep;

    #[test]
    fn pane_cursor_parser_requires_exact_three_numeric_fields() {
        assert_eq!(
            parse_pane_cursor("2,20,1\n").unwrap(),
            PaneCursor {
                x: 2,
                y: 20,
                visible: true,
                attached_clients: 0,
            }
        );
        for bad in ["2,20", "2,20,1,extra", "x,20,1", "2,20,2", ""] {
            assert!(parse_pane_cursor(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_detach_on_destroy_on_takes_the_client_down_with_the_session() {
        assert!(detach_on_destroy_exits("on\n"));
        for other in ["off", "previous", "next", "no-detached", "", "garbage"] {
            assert!(!detach_on_destroy_exits(other), "{other:?}");
        }
    }

    #[test]
    fn a_client_is_found_by_its_pid_only() {
        let raw = "4242,soul-a\n777,summon-wt-x\n";
        assert_eq!(
            parse_client_session(raw, 777).as_deref(),
            Some("summon-wt-x")
        );
        assert_eq!(parse_client_session(raw, 4242).as_deref(), Some("soul-a"));
        assert_eq!(parse_client_session(raw, 42), None, "前缀相同的 pid 不算");
        assert_eq!(parse_client_session("", 777), None);
    }

    #[test]
    fn only_writable_clients_count_as_someone_at_the_keyboard() {
        let none = BTreeSet::new();
        assert_eq!(parse_writable_clients("", &none).unwrap(), 0);
        assert_eq!(
            parse_writable_clients("1,777\n", &none).unwrap(),
            0,
            "本地只读客户端打不了字"
        );
        assert_eq!(parse_writable_clients("0,1\n1,2\n0,3\n", &none).unwrap(), 2);
    }

    /// summon 自己的网页客户端(登记表里的进程号)不算有人在键盘前,别的可写客户端照常算。
    #[test]
    fn summon_web_clients_are_not_counted_as_someone_at_the_keyboard() {
        let web = BTreeSet::from([4242]);
        assert_eq!(
            parse_writable_clients("0,4242\n", &web).unwrap(),
            0,
            "只有网页客户端"
        );
        assert_eq!(
            parse_writable_clients("0,4242\n0,777\n", &web).unwrap(),
            1,
            "网页客户端加一个本地可写客户端"
        );
        assert_eq!(
            parse_writable_clients("0,4242\n1,777\n", &web).unwrap(),
            0,
            "网页客户端加一个本地只读客户端"
        );
        assert_eq!(
            parse_writable_clients("0,42\n0,424\n", &web).unwrap(),
            2,
            "按整个进程号认,不按前缀"
        );
        assert_eq!(
            parse_writable_clients("0,777\n", &BTreeSet::from([4242, 5151])).unwrap(),
            1,
            "登记了但不在列表里的进程号不影响计数"
        );
    }

    /// 字段认不出一律 Err:注入闸据此 fail-closed,而不是猜一个客户端数。
    #[test]
    fn an_unreadable_client_list_is_an_error() {
        let web = BTreeSet::from([4242]);
        for bad in [
            "0\n",
            "1\n",
            "yes,4242\n",
            "0,\n",
            ",4242\n",
            "0,abc\n",
            "0,-1\n",
            "0,4242,x\n",
            "0,777\n2,4242\n",
        ] {
            assert!(parse_writable_clients(bad, &web).is_err(), "{bad:?}");
        }
    }

    /// 登记随 spawn 一起做,析构即注销;spawn 失败什么都不登记。用 `sleep` 当客户端,不碰 tmux。
    #[test]
    fn a_web_client_is_registered_from_spawn_until_its_registration_drops() {
        let (mut child, registration) = spawn_web_client(Command::new("sleep").arg("30")).unwrap();
        let pid = child.id();
        let registered = web_clients().contains(&pid);
        let _ = child.kill();
        let _ = child.wait();
        let still_registered_after_exit = web_clients().contains(&pid);
        drop(registration);
        assert!(registered, "spawn 返回时已登记");
        assert!(still_registered_after_exit, "登记跟着持有者走,不跟着进程");
        assert!(!web_clients().contains(&pid), "析构即注销");
        assert!(spawn_web_client(&mut Command::new("/nonexistent/summon-no-such-bin")).is_err());
    }

    /// 窗口 id 只认 `@` + 十进制数字。认不出就报错:窗口级的钉住与恢复都拿它当 `-t` 目标,
    /// 一段来路不明的串送进去就是写到别的窗口上,而 tmux 照样返回成功。
    #[test]
    fn a_window_id_is_an_at_sign_followed_by_digits() {
        assert_eq!(WindowId::parse("@12\n").unwrap().as_target(), "@12");
        assert_eq!(WindowId::parse("@0").unwrap().to_string(), "@0");
        for bad in ["", "@", "12", "@1a", "%3", "$1", "@1 @2", "=soul-x:"] {
            assert!(WindowId::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn window_geometry_counts_status_lines() {
        let g = parse_window_geometry("155,58,on\n").unwrap();
        assert_eq!((g.cols, g.rows, g.status_lines), (155, 58, 1));
        assert_eq!(parse_window_geometry("80,24,off").unwrap().status_lines, 0);
        assert_eq!(parse_window_geometry("80,24,3").unwrap().status_lines, 3);
        assert!(parse_window_geometry("80,24").is_err());
    }

    #[test]
    fn only_destroy_unattached_off_survives_the_last_detach() {
        assert!(!last_detach_destroys("off\n"));
        assert!(!last_detach_destroys("off"));
        for value in ["on\n", "keep-last\n", "keep-group\n", "", "\n", "OFF", "1"] {
            assert!(last_detach_destroys(value), "{value:?} 应当按会销毁处理");
        }
    }

    /// `-u`:客户端按 UTF-8 画屏,不看区域变量(launchd 起的 daemon 常常一个都没有)。
    #[test]
    fn attach_command_targets_the_exact_session_forces_utf8_and_marks_read_only() {
        let t = Tmux::new("soul-x".to_string());
        let args = |c: &std::process::Command| {
            c.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            args(&t.attach_command(true)),
            ["-u", "attach-session", "-f", "read-only", "-t", "=soul-x"]
        );
        assert_eq!(
            args(&t.attach_command(false)),
            ["-u", "attach-session", "-t", "=soul-x"]
        );
    }

    #[test]
    fn page_input_goes_to_this_pane_as_hex_bytes_in_bounded_chunks() {
        let t = Tmux::new("soul-x".to_string());
        assert!(t.send_bytes_argvs(b"").is_empty(), "没有输入就不起 tmux");
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            t.send_bytes_argvs(b"\x02:\x1b\r"),
            vec![owned(&[
                "copy-mode",
                "-q",
                "-t",
                "=soul-x:",
                ";",
                "send-keys",
                "-t",
                "=soul-x:",
                "-H",
                "02",
                "3a",
                "1b",
                "0d",
            ])]
        );
        let hex_of = |argv: &[String]| {
            let at = argv.iter().position(|a| a == "-H").expect("-H");
            argv[at + 1..].to_vec()
        };
        let zh = t.send_bytes_argvs("中".as_bytes());
        assert_eq!(hex_of(&zh[0]), owned(&["e4", "b8", "ad"]));
        let every: Vec<u8> = (0..=255).collect();
        let all = t.send_bytes_argvs(&every);
        assert_eq!(all.len(), 1);
        let expected: Vec<String> = (0..=255u8).map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex_of(&all[0]), expected, "00–ff 每个字节原样");
        let big = t.send_bytes_argvs(&[b'a'; 2500]);
        assert_eq!(
            big.iter().map(|a| hex_of(a).len()).collect::<Vec<_>>(),
            [1024, 1024, 452],
            "按序分块,每块不超过 1024 字节"
        );
    }

    fn mouse_on(width: u16, height: u16) -> PaneMouse {
        PaneMouse {
            in_mode: false,
            sgr: true,
            any: true,
            button: false,
            standard: true,
            synchronized: false,
            width,
            height,
        }
    }

    #[test]
    fn pane_mouse_parser_reads_the_id_flags_and_size_and_rejects_anything_else() {
        let (id, mouse) = parse_pane_mouse("%12,0,1,1,0,1,0,81,25\n").unwrap();
        assert_eq!(id.as_target(), "%12");
        assert_eq!(mouse, mouse_on(81, 25));
        let (_, in_mode) = parse_pane_mouse("%3,1,0,0,1,0,0,1,1").unwrap();
        assert!(in_mode.in_mode && !in_mode.sgr && !in_mode.any && in_mode.button);
        let (_, synced) = parse_pane_mouse("%3,0,1,1,0,1,1,80,24").unwrap();
        assert!(synced.synchronized && !synced.takes_wheel());
        for bad in [
            "",
            "%12,0,1,1,0,1,0,81",
            "%12,0,1,1,0,1,0,81,25,1",
            "%12,0,1,1,0,1,81,25",
            "12,0,1,1,0,1,0,81,25",
            "%12,2,1,1,0,1,0,81,25",
            "%12,,1,1,0,1,0,81,25",
            "%12,0,1,1,0,on,0,81,25",
            "%12,0,1,1,0,1,on,81,25",
            "%12,0,1,1,0,1,0,-1,25",
            "%12,0,1,1,0,1,0,81,x",
        ] {
            assert!(parse_pane_mouse(bad).is_err(), "{bad:?}");
        }
    }

    /// 窗格 id 要拼进 tmux 解析的命令串:只认 `%` + 数字,别的一律拒。
    #[test]
    fn a_pane_id_is_a_percent_sign_followed_by_digits() {
        assert_eq!(PaneId::parse("%0\n").unwrap().as_target(), "%0");
        assert_eq!(PaneId::parse("%123").unwrap().as_target(), "%123");
        for bad in ["", "%", "12", "%1a", "%1;ls", "%1 x", "$1", "%1'"] {
            assert!(PaneId::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// 滚轮事件送到查到的那个窗格,送之前由 tmux 按同一条判据再判一次,且绝不经 `copy-mode -q`:
    /// 窗格在模式里就不送,本地用户的 copy-mode 是他的。
    #[test]
    fn wheel_events_go_to_that_pane_behind_a_tmux_side_gate_and_never_via_copy_mode() {
        let pane = PaneId::parse("%7").unwrap();
        let events = WheelEvents::for_pane(&mouse_on(80, 24), -1).unwrap();
        let hex = "1b 5b 3c 36 34 3b 34 30 3b 31 32 4d";
        assert_eq!(
            pane.send_wheel_argv(&events, false),
            owned(&[
                "if-shell",
                "-F",
                "-t",
                "%7",
                WHEEL_GATE,
                &format!("send-keys -t %7 -H {hex}"),
                "display-message -p summon-wheel-refused",
            ])
        );
        let retry = pane.send_wheel_argv(&events, true);
        assert_eq!(retry[5], format!("send-keys -c '' -t %7 -H {hex}"));
        assert_eq!(retry[6], "display-message -p summon-wheel-refused");
        assert_eq!(
            WHEEL_GATE,
            "#{?synchronize-panes,0,#{?pane_in_mode,0,#{?mouse_sgr_flag,#{?mouse_any_flag,1,\
#{?mouse_button_flag,1,#{?mouse_standard_flag,1,0}}},0}}}"
        );
        for without_client in [false, true] {
            let argv = pane.send_wheel_argv(&events, without_client);
            assert!(!argv.iter().any(|a| a.contains("copy-mode")), "{argv:?}");
        }
    }

    /// 闸不成立时 `if-shell` 的另一支打出记号,送出去的那一支什么都不打;打了别的也不算送出去。
    #[test]
    fn only_a_silent_if_shell_counts_as_a_sent_wheel() {
        assert_eq!(wheel_send_outcome(b""), WheelSend::Sent);
        assert_eq!(wheel_send_outcome(b"\n"), WheelSend::Sent);
        assert_eq!(
            wheel_send_outcome(b"summon-wheel-refused\n"),
            WheelSend::Refused
        );
        assert_eq!(wheel_send_outcome(b"something else"), WheelSend::Refused);
        assert_eq!(WHEEL_REFUSED, "summon-wheel-refused");
    }

    /// tmux 展开的 `WHEEL_GATE` 在给定窗格上的取值。
    fn wheel_gate_on(pane: &PaneId) -> String {
        let out = Command::new("tmux")
            .args(["display-message", "-p", "-t", pane.as_target(), WHEEL_GATE])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// 真 tmux:tmux 那边的闸与 `PaneMouse::takes_wheel` 是同一条判据。窗格里的程序开关几种鼠标
    /// 模式,每种组合都在模式外、copy-mode 里、窗口开着 synchronize-panes 时各比一次。
    #[test]
    #[ignore]
    fn the_tmux_side_wheel_gate_agrees_with_takes_wheel() {
        let session = crate::test_support::ThrowawaySession::start("summon-wheel-gate");
        let t = &session.0;
        let reset = r"\033[?1000l\033[?1002l\033[?1003l\033[?1006l";
        let mut seen = Vec::new();
        for (modes, expected) in [
            ("", false),
            (r"\033[?1006h", false),
            (r"\033[?1000h", false),
            (r"\033[?1000h\033[?1006h", true),
            (r"\033[?1002h\033[?1006h", true),
            (r"\033[?1003h\033[?1006h", true),
        ] {
            t.send_text(&format!("printf '{reset}{modes}'")).unwrap();
            t.send_enter().unwrap();
            sleep(Duration::from_millis(400));
            let (pane, mouse) = t.pane_mouse().unwrap();
            let gate = wheel_gate_on(&pane);
            Command::new("tmux")
                .args(["copy-mode", "-t", pane.as_target()])
                .status()
                .unwrap();
            let (_, in_mode) = t.pane_mouse().unwrap();
            let gate_in_mode = wheel_gate_on(&pane);
            t.exit_copy_mode();
            set_synchronized(t, true);
            let (_, synced) = t.pane_mouse().unwrap();
            let gate_synced = wheel_gate_on(&pane);
            set_synchronized(t, false);
            seen.push((
                modes,
                expected,
                mouse,
                gate,
                [(in_mode, gate_in_mode), (synced, gate_synced)],
            ));
        }
        for (modes, expected, mouse, gate, refused) in seen {
            assert_eq!(mouse.takes_wheel(), expected, "{modes}: {mouse:?}");
            assert_eq!(gate, if expected { "1" } else { "0" }, "{modes}");
            let [(in_mode, gate_in_mode), (synced, gate_synced)] = refused;
            assert!(in_mode.in_mode, "{modes}: 前提:进了 copy-mode");
            assert!(!in_mode.takes_wheel(), "{modes}");
            assert_eq!(gate_in_mode, "0", "{modes}");
            assert!(synced.synchronized, "{modes}: 前提:开了 synchronize-panes");
            assert!(!synced.takes_wheel(), "{modes}");
            assert_eq!(gate_synced, "0", "{modes}");
        }
    }

    /// 给 `t` 的当前窗口开关 `synchronize-panes`。
    fn set_synchronized(t: &Tmux, on: bool) {
        let target = format!("={}:", t.session);
        let status = Command::new("tmux")
            .args(["set-option", "-w", "-t", &target, "synchronize-panes"])
            .arg(if on { "on" } else { "off" })
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// 测试在同一窗口里另开的窗格。析构时发 C-c 让里面的程序退出,窗格随之关掉。
    struct SiblingPane(PaneId);

    impl Drop for SiblingPane {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["send-keys", "-t", self.0.as_target(), "C-c"])
                .status();
        }
    }

    /// 真 tmux:窗口开着 synchronize-panes 时,tmux 会把送进窗格的字节照抄给隔壁的同步窗格,不管
    /// 隔壁开没开鼠标汇报。开着就一个字节都不送:Rust 这边的判据不收;绕过它直接送,tmux 那边的
    /// 闸也拦下——两个窗格都什么都没收到。关掉之后照常只到目标窗格。
    #[test]
    #[ignore]
    fn a_synchronized_pane_gets_no_wheel_events_and_neither_do_its_siblings() {
        let rec = crate::test_support::Recorder::start("summon-wheel-sync", true);
        let t = rec.tmux();
        let dir = tempfile::tempdir().unwrap();
        let sibling_file = dir.path().join("sibling.bin");
        let split = Command::new("tmux")
            .args(["split-window", "-d", "-P", "-F", "#{pane_id}", "-t"])
            .arg(format!("={}:", t.session))
            .arg(format!(
                "stty -icanon -echo -icrnl -ixon min 1 time 0; exec cat > '{}'",
                sibling_file.display()
            ))
            .output()
            .unwrap();
        let sibling = SiblingPane(PaneId::parse(&String::from_utf8_lossy(&split.stdout)).unwrap());
        sleep(Duration::from_millis(600));
        let sibling_got = || std::fs::read(&sibling_file).unwrap_or_default();
        let (pane, open) = t.pane_mouse().unwrap();
        let events = WheelEvents::for_pane(&open, -2).unwrap();

        set_synchronized(t, true);
        let (_, synced) = t.pane_mouse().unwrap();
        let sent = t.send_wheel(&pane, &events);
        let got_synced = rec.wait_for(b"", Duration::from_millis(800));
        let sibling_synced = sibling_got();
        set_synchronized(t, false);
        let sent_after = t.send_wheel(&pane, &events);
        let got_after = rec.wait_for(events.as_bytes(), Duration::from_secs(3));
        let sibling_after = sibling_got();
        drop(sibling);

        assert_ne!(
            pane,
            PaneId::parse("%0").unwrap(),
            "前提:查到的是记录程序那个窗格"
        );
        assert!(open.takes_wheel(), "前提:没开同步时收滚轮");
        assert!(synced.synchronized && !synced.takes_wheel(), "{synced:?}");
        assert_eq!(sent.ok(), Some(WheelSend::Refused), "闸不过要报出来");
        assert_eq!(got_synced, b"", "目标窗格");
        assert_eq!(sibling_synced, b"", "隔壁窗格");
        assert_eq!(sent_after.ok(), Some(WheelSend::Sent));
        assert_eq!(got_after, events.as_bytes(), "关掉同步后照常到达");
        assert_eq!(sibling_after, b"", "隔壁窗格从头到尾什么都没收到");
    }

    /// 真 tmux:只读客户端挂着(且是整个服务上最近活跃的客户端)时,`send-keys` 被整条拒绝;网页的
    /// 滚轮带 `-c ''` 重跑,照样送到。重跑的那条命令闸不过时,记号同样打到标准输出。
    #[test]
    #[ignore]
    fn wheel_events_still_arrive_while_a_read_only_client_is_attached() {
        let rec = crate::test_support::Recorder::start("summon-wheel-ro", true);
        let t = rec.tmux();
        let _ro = AttachedClient::spawn(t, true);
        sleep(Duration::from_millis(800));
        let plain = Command::new("tmux")
            .args(["display-message", "-p", "-t"])
            .arg(format!("={}:", t.session))
            .arg("#{session_attached}")
            .output()
            .unwrap();
        let (pane, mouse) = t.pane_mouse().unwrap();
        let events = WheelEvents::for_pane(&mouse, -2).unwrap();
        let refused = Command::new("tmux")
            .args(["if-shell", "-F", "-t", pane.as_target(), "1"])
            .arg(format!("send-keys -t {} -H 00", pane.as_target()))
            .output()
            .unwrap();
        let plain_was_refused = refused_for_read_only_client(&refused.stderr);
        assert!(
            plain_was_refused || refused.status.success(),
            "前提:不带 -c 的 send-keys 既没被只读客户端拒绝、也没成功:{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        // tmux 3.7 起上面的探针被拒,旧版本则会真的送进一个 NUL。先冻结现有前缀,
        // 后面的断言只检查滚轮事件恰好追加一次,两类版本都覆盖各自的生产路径。
        let before = if plain_was_refused {
            rec.wait_for(b"", Duration::from_millis(300))
        } else {
            rec.wait_for(&[0], Duration::from_secs(3))
        };
        let sent = t.send_wheel(&pane, &events);
        let expected = [before.as_slice(), events.as_bytes()].concat();
        let got = rec.wait_for(&expected, Duration::from_secs(3));
        Command::new("tmux")
            .args(["copy-mode", "-t", pane.as_target()])
            .status()
            .unwrap();
        let retry_refused = Command::new("tmux")
            .args(pane.send_wheel_argv(&events, true))
            .output()
            .unwrap();
        let refused_in_mode = t.send_wheel(&pane, &events);
        t.exit_copy_mode();
        let got_after = rec.wait_for(b"", Duration::from_millis(500));
        assert_eq!(
            String::from_utf8_lossy(&plain.stdout).trim(),
            "1",
            "前提:挂着一个客户端"
        );
        assert_eq!(
            before,
            if plain_was_refused {
                Vec::new()
            } else {
                vec![0]
            },
            "只允许旧版 tmux 的成功探针多送一个 NUL"
        );
        assert_eq!(sent.ok(), Some(WheelSend::Sent));
        assert_eq!(got, expected);
        assert!(retry_refused.status.success());
        assert_eq!(
            wheel_send_outcome(&retry_refused.stdout),
            WheelSend::Refused,
            "{}",
            String::from_utf8_lossy(&retry_refused.stdout)
        );
        assert_eq!(refused_in_mode.ok(), Some(WheelSend::Refused));
        assert_eq!(got_after, expected, "闸不过的那两次什么都没送");
    }

    /// 真 tmux:查过之后窗格变了(本地有人刚进 copy-mode、程序关了鼠标汇报),送出去的滚轮事件被
    /// tmux 那边的闸拦下:一个字节都不进程序,copy-mode 也不被取消,结果报成闸不过(这一下算丢掉,
    /// 不进净格数)。窗格恢复后照常到达,报成送出去。
    #[test]
    #[ignore]
    fn wheel_events_are_held_back_when_the_pane_changed_after_the_query() {
        let rec = crate::test_support::Recorder::start("summon-wheel-race", true);
        let t = rec.tmux();
        let (pane, mouse) = t.pane_mouse().unwrap();
        let events = WheelEvents::for_pane(&mouse, -2).unwrap();
        Command::new("tmux")
            .args(["copy-mode", "-t", pane.as_target()])
            .status()
            .unwrap();
        let sent_in_mode = pane.send_wheel(&events);
        let got_in_mode = rec.wait_for(b"", Duration::from_millis(800));
        let still_in_mode = t.pane_mouse().unwrap().1.in_mode;
        t.exit_copy_mode();
        let sent_after = pane.send_wheel(&events);
        let got_after = rec.wait_for(events.as_bytes(), Duration::from_secs(3));

        let plain = crate::test_support::Recorder::start("summon-wheel-plain", false);
        let (plain_pane, plain_mouse) = plain.tmux().pane_mouse().unwrap();
        let sent_plain = plain_pane.send_wheel(&events);
        let got_plain = plain.wait_for(b"", Duration::from_millis(800));

        assert_eq!(sent_in_mode.ok(), Some(WheelSend::Refused));
        assert!(got_in_mode.is_empty(), "{got_in_mode:?}");
        assert!(still_in_mode, "copy-mode 不该被取消");
        assert_eq!(sent_after.ok(), Some(WheelSend::Sent));
        assert_eq!(got_after, events.as_bytes(), "窗格恢复后照常到达");
        assert!(!plain_mouse.takes_wheel(), "前提:程序没开鼠标汇报");
        assert_eq!(sent_plain.ok(), Some(WheelSend::Refused));
        assert!(got_plain.is_empty(), "{got_plain:?}");
    }

    /// 真 tmux:滚轮也在会话的网页输入锁里送。锁被拿着(比如可操作页一段长输入正分块送)时,别的
    /// 连接的滚轮一个字节都进不了窗格;放开之后才到。
    #[test]
    #[ignore]
    fn wheel_events_wait_for_the_session_input_lock() {
        let rec = crate::test_support::Recorder::start("summon-wheel-lock", true);
        let t = rec.tmux();
        let (pane, mouse) = t.pane_mouse().unwrap();
        let events = WheelEvents::for_pane(&mouse, -2).unwrap();
        let lock = input_lock(&t.session);
        let held = lock.lock().unwrap();
        let worker = {
            let session = t.session.clone();
            let (pane, events) = (pane.clone(), events.clone());
            std::thread::spawn(move || Tmux::new(session).send_wheel(&pane, &events))
        };
        let got_while_held = rec.wait_for(b"", Duration::from_millis(800));
        drop(held);
        let sent = worker.join().unwrap();
        let got_after = rec.wait_for(events.as_bytes(), Duration::from_secs(3));
        assert_eq!(got_while_held, b"", "锁被拿着时不该送进去");
        assert_eq!(sent.ok(), Some(WheelSend::Sent));
        assert_eq!(got_after, events.as_bytes(), "放开之后送到");
    }

    /// 真 tmux:按窗格 id 查状态、抓画面只认那一个窗格;窗格没了就 Err,不拿别的窗格或空串顶替。
    #[test]
    #[ignore]
    fn a_pane_is_queried_and_captured_by_its_id_and_a_gone_pane_is_an_error() {
        let session = crate::test_support::ThrowawaySession::start("summon-pane-id");
        let t = &session.0;
        t.send_text("echo PANE_$((6*7))").unwrap();
        t.send_enter().unwrap();
        sleep(Duration::from_millis(400));
        let (pane, _) = t.pane_mouse().unwrap();
        let mouse = pane.mouse();
        let screen = pane.capture_visible();
        let gone = PaneId::parse("%999999").unwrap();
        assert!(mouse.is_ok(), "{mouse:?}");
        assert!(
            screen.as_deref().is_ok_and(|s| s.contains("PANE_42")),
            "{screen:?}"
        );
        assert!(gone.mouse().is_err());
        assert!(gone.capture_visible().is_err());
    }

    /// 网页往同一个会话送的输入排队,别的会话不受影响;没人拿着的锁会被清掉。
    #[test]
    fn web_input_to_one_session_is_serialized_and_other_sessions_are_not_held_up() {
        let name = format!("summon-input-lock-{}", std::process::id());
        let a = input_lock(&name);
        let b = input_lock(&name);
        let other = input_lock(&format!("{name}-other"));
        assert!(Arc::ptr_eq(&a, &b), "同一会话同一把锁");
        assert!(!Arc::ptr_eq(&a, &other));
        let held = a.lock().unwrap();
        assert!(b.try_lock().is_err(), "同一会话要等");
        assert!(other.try_lock().is_ok(), "别的会话不等");
        drop(held);
        drop((a, b, other));
        let _trigger = input_lock(&format!("{name}-trigger"));
        let locks = INPUT_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(!locks.contains_key(&name), "没人拿着的锁被清掉");
    }

    /// 按键整段在锁里送:锁被别人拿着时,`send_bytes` 等到放开才动(空输入不起 tmux,测试不碰任何
    /// tmux 服务)。
    #[test]
    fn keystrokes_wait_for_the_session_input_lock() {
        let name = format!("summon-input-wait-{}", std::process::id());
        let lock = input_lock(&name);
        let held = lock.lock().unwrap();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = {
            let done = Arc::clone(&done);
            let name = name.clone();
            std::thread::spawn(move || {
                Tmux::new(name).send_bytes(b"").unwrap();
                done.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };
        sleep(Duration::from_millis(200));
        let waited = !done.load(std::sync::atomic::Ordering::SeqCst);
        drop(held);
        worker.join().unwrap();
        assert!(waited, "锁被拿着时 send_bytes 不该跑完");
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// 真 tmux:只读客户端不算「有人在键盘前」。
    #[test]
    #[ignore]
    fn a_read_only_client_is_not_counted_as_writable() {
        let session = crate::test_support::ThrowawaySession::start("summon-wt-test");
        let t = &session.0;
        let _ro = AttachedClient::spawn(t, true);
        sleep(Duration::from_millis(800));
        assert_eq!(t.writable_clients().unwrap(), 0);
        let _rw = AttachedClient::spawn(t, false);
        sleep(Duration::from_millis(800));
        assert_eq!(t.writable_clients().unwrap(), 1);
    }

    #[test]
    fn a_refused_send_keys_is_rerun_without_a_target_client() {
        assert!(refused_for_read_only_client(b"client is read-only\n"));
        assert!(!refused_for_read_only_client(b"can't find session: x\n"));
        assert!(!refused_for_read_only_client(b""));
        assert_eq!(
            without_target_client(&owned(&["send-keys", "-t", "=s:", "-l", "--", "send-keys"])),
            owned(&["send-keys", "-c", "", "-t", "=s:", "-l", "--", "send-keys"]),
            "只改命令位置上的 send-keys,正文不动"
        );
        assert_eq!(
            without_target_client(&owned(&[
                "copy-mode",
                "-q",
                "-t",
                "=s:",
                ";",
                "send-keys",
                "-t",
                "=s:",
                "-H",
                "41"
            ])),
            owned(&[
                "copy-mode",
                "-q",
                "-t",
                "=s:",
                ";",
                "send-keys",
                "-c",
                "",
                "-t",
                "=s:",
                "-H",
                "41"
            ])
        );
    }

    /// 各块的字节(按 `send_chunk_ends` 切)。
    fn chunks_of(bytes: &[u8], max: usize) -> Vec<Vec<u8>> {
        let mut start = 0;
        send_chunk_ends(bytes, max)
            .into_iter()
            .map(|end| {
                let chunk = bytes[start..end].to_vec();
                start = end;
                chunk
            })
            .collect()
    }

    /// 分块不把一个 UTF-8 字符或一个转义序列劈进两次 `send-keys`:劈开的 `\e[201~` 在两次写之间
    /// 可能被 claude 读成一个单独的 Esc。
    #[test]
    fn page_input_chunks_never_split_a_character_or_an_escape_sequence() {
        let a = |n: usize| vec![b'a'; n];
        assert!(send_chunk_ends(b"", 8).is_empty());
        assert_eq!(chunks_of(&a(20), 8), [a(8), a(8), a(4)]);

        let zh = [a(7), "中b".as_bytes().to_vec()].concat();
        assert_eq!(
            chunks_of(&zh, 8),
            [a(7), "中b".as_bytes().to_vec()],
            "中文不劈开"
        );

        let end_paste = b"\x1b[201~".to_vec();
        let straddle = [a(5), end_paste.clone(), b"z".to_vec()].concat();
        assert_eq!(
            chunks_of(&straddle, 8),
            [a(5), [end_paste.clone(), b"z".to_vec()].concat()],
            "粘贴结束标记跨界时整段挪到下一块"
        );
        let exact = [a(2), end_paste.clone()].concat();
        assert_eq!(
            chunks_of(&exact, 8),
            std::slice::from_ref(&exact),
            "恰好在界上收尾的不动"
        );

        let lone_esc = [a(7), b"\x1b".to_vec(), b"x".to_vec()].concat();
        assert_eq!(
            chunks_of(&lone_esc, 8),
            [a(7), b"\x1bx".to_vec()],
            "Alt 组合键不劈开"
        );
        let ss3 = [a(7), b"\x1bOA".to_vec()].concat();
        assert_eq!(chunks_of(&ss3, 8), [a(7), b"\x1bOA".to_vec()]);
        let osc = [a(3), b"\x1b]0;hi\x07".to_vec()].concat();
        assert_eq!(chunks_of(&osc, 8), [a(3), b"\x1b]0;hi\x07".to_vec()]);
        let st = [a(1), b"\x1bP1\x1b\\".to_vec()].concat();
        assert_eq!(
            chunks_of(&st, 5),
            [a(1), b"\x1bP1\x1b\\".to_vec()],
            "ST 跨界"
        );

        let long_osc = [b"\x1b]".to_vec(), vec![b'x'; 20]].concat();
        assert_eq!(
            chunks_of(&long_osc, 8)
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [8, 8, 6],
            "比一块还长的序列只能按块切,但不能卡住"
        );

        // 粘贴一大段中文:每个切点都落在字符边界上,每块都不超长。
        let paste = [
            b"\x1b[200~".to_vec(),
            "粘贴的中文内容".repeat(100).into_bytes(),
            end_paste,
        ]
        .concat();
        let mut start = 0;
        for end in send_chunk_ends(&paste, 1024) {
            assert!(end - start <= 1024 && end > start);
            assert!(
                std::str::from_utf8(&paste[start..end]).is_ok(),
                "块内是完整字符"
            );
            start = end;
        }
        assert_eq!(start, paste.len());
    }

    /// 真 tmux:只读客户端挂着(且是整个服务上最近活跃的客户端)时,summon 自己的注入与网页输入
    /// 仍送得进窗格。tmux 3.7 起 `send-keys` 的目标客户端是只读的就整条拒绝。
    #[test]
    #[ignore]
    fn keys_still_reach_the_pane_while_a_read_only_client_is_attached() {
        let session = crate::test_support::ThrowawaySession::start("summon-wt-rosend");
        let t = &session.0;
        let _ro = AttachedClient::spawn(t, true);
        sleep(Duration::from_millis(800));
        let _ = t.send_text("echo TXT_$((6*7))");
        let _ = t.send_enter();
        let bytes = t.send_bytes("echo 字节_$((6*7))\r".as_bytes());
        sleep(Duration::from_millis(800));
        let pane = t.capture().unwrap();
        assert!(pane.contains("TXT_42"), "summon 自己的注入被拒了:{pane}");
        assert!(bytes.is_ok(), "{bytes:?}");
        assert!(pane.contains("字节_42"), "网页输入被拒了:{pane}");
    }

    /// **tmux `-F` 格式串里一个控制字符都不许有。**
    ///
    /// 由来(2026-08-15 真机):三处格式串用 `\t` 分隔字段,而 tmux 3.7b 展开 `-F` 时把不可打印
    /// 字符改写成 `_` 再输出 —— 代码写 `\t`,拿回来的是下划线。两个解析器按 `\t` 切分于是全部
    /// 判 Err,而调用方把 Err 当「读不到」静默降级:注入闸的 placeholder 探测第一步就退出,空
    /// 输入框被一律判成「有草稿」,每条消息都被拒。**编译通过、测试全绿、线上全坏。**
    ///
    /// 这条守护扫本文件源码,把「格式串必须可打印」钉成结构不变量 —— 换个分隔符、加一处格式串
    /// 都还会被拦住,而不是等下一次 tmux 升级再炸一遍。
    #[test]
    fn tmux_format_strings_contain_no_control_characters() {
        let src = include_str!("tmux.rs");
        for (lineno, line) in src.lines().enumerate() {
            if !line.contains("#{") {
                continue;
            }
            // 只看字符串字面量里的转义序列:源码里的 `\t` 是两个字符,不是真 TAB。
            for bad in ["\\t", "\\n", "\\r", "\\0"] {
                assert!(
                    !line.contains(bad),
                    "tmux 格式串不得含控制字符({bad}):第 {} 行 `{}`。\
                     tmux 会把它改写成 `_`,解析必失败(见 TMUX_FORMAT_SEP)",
                    lineno + 1,
                    line.trim()
                );
            }
        }
    }

    /// 分隔符必须是**任何字段都不可能出现**的可打印字符。
    #[test]
    fn the_format_separator_can_never_collide_with_a_field() {
        assert!(
            TMUX_FORMAT_SEP.is_ascii() && !TMUX_FORMAT_SEP.is_control(),
            "分隔符必须可打印 ASCII"
        );
        // session_name 的合法字符集(见 validate_tmux_name)不含分隔符。
        assert!(
            !TMUX_FORMAT_SEP.is_ascii_alphanumeric()
                && TMUX_FORMAT_SEP != '-'
                && TMUX_FORMAT_SEP != '_',
            "分隔符不能落在 session_name 的合法字符集里"
        );
        // 其余三个字段是 `$`/`%` 前缀的数字。
        assert!(TMUX_FORMAT_SEP != '$' && TMUX_FORMAT_SEP != '%');
    }
    use std::time::Duration;

    #[test]
    fn handoff_staging_names_and_markers_are_fail_closed() {
        let valid = format!("{HANDOFF_STAGING_PREFIX}{}", uuid::Uuid::new_v4());
        assert!(validate_staging_name(&valid).is_ok());
        for invalid in [
            "soul-user",
            "summon-bridge-handoff-not-a-uuid",
            "summon-bridge-handoff-../../soul-user",
        ] {
            assert!(validate_staging_name(invalid).is_err());
        }
        assert!(validate_marker(&"a".repeat(64)).is_ok());
        assert!(validate_marker("short").is_err());
        assert!(validate_marker(&format!("{}z", "a".repeat(63))).is_err());
    }

    #[test]
    fn tmux_identity_parser_rejects_partial_output() {
        let marker = "a".repeat(64);
        assert_eq!(
            parse_identity("name,$1,123,%2\n", &marker).unwrap(),
            TmuxSessionIdentity {
                name: "name".into(),
                session_id: "$1".into(),
                session_created: 123,
                pane_id: "%2".into(),
                ownership_marker: marker,
            }
        );
        assert!(parse_identity("name $1 123 %2", &"a".repeat(64)).is_err());
        // tmux 把控制字符改写成 `_` 之后的真实形态:必须判 Err,不能碰巧解析成功。
        assert!(parse_identity("name_$1_123_%2", &"a".repeat(64)).is_err());
    }

    #[test]
    fn strict_probe_only_accepts_explicit_missing_errors() {
        assert!(classify_tmux_probe(true, b"").unwrap());
        assert!(!classify_tmux_probe(false, b"no server running on /tmp/tmux/default\n").unwrap());
        assert!(!classify_tmux_probe(false, b"can't find session: x\n").unwrap());
        assert!(!classify_tmux_probe(
            false,
            b"error connecting to /private/tmp/tmux-501/default (No such file or directory)\n"
        )
        .unwrap());
        assert!(!classify_tmux_probe(
            false,
            b"error connecting to /tmp/tmux-501/default (Connection refused)\n"
        )
        .unwrap());
        assert!(classify_tmux_probe(false, b"no such file or directory\n").is_err());
        assert!(classify_tmux_probe(
            false,
            b"error connecting to /tmp/tmux-501/default (Permission denied)\n"
        )
        .is_err());
        assert!(classify_tmux_probe(
            false,
            b"error connecting to /tmp/tmux-501/default (Permission denied)\n\
              retry failed (No such file or directory)\n"
        )
        .is_err());
        assert!(classify_tmux_probe(false, b"permission denied\n").is_err());
        assert!(classify_tmux_probe(false, &[0xff]).is_err());
    }

    /// staging 的 argv 里只该出现 env **文件路径**,绝不出现任何键值对——那条 argv 会成为
    /// tmux server 进程的命令行(macOS 上 tmux 改不掉自己 argv、server 又常驻),同 uid 的
    /// 任何 agent 都能 `ps` 读到。handoff 的 env 里还有一次性 token,更不能落 argv。
    #[test]
    fn staging_argv_carries_env_file_path_not_values() {
        let env_file = Path::new("/tmp/summon-test/launch.env");
        let argv = staging_new_session_argv(
            "summon-bridge-handoff-x",
            Path::new("/tmp/wd"),
            env_file,
            Path::new("/usr/local/bin/claude"),
            &["--resume".to_string(), "sid-1".to_string()],
        );
        let flat: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(flat.contains(&"/tmp/summon-test/launch.env".to_string()));
        assert!(flat.contains(&"/usr/local/bin/claude".to_string()));
        assert!(flat.contains(&"sid-1".to_string()));
        // env -i 仍在最前(清空继承),但后面不再跟任何 K=V。
        let env_at = flat.iter().position(|a| a == "/usr/bin/env").unwrap();
        assert_eq!(flat[env_at + 1], "-i");
        assert_eq!(flat[env_at + 2], "/bin/sh");
        for arg in &flat {
            assert!(
                !arg.contains("FEISHU_APP_SECRET="),
                "argv 里出现了 env 键值对: {arg}"
            );
        }
    }

    /// 真机 proof(本任务唯一重要的验收点):在**自己的私有 tmux socket** 上按新方案起一个会话,
    /// 断言 ① server 进程的 argv 里没有 secret;② 子进程环境里**有** secret(功能没坏);
    /// ③ 一次性 env 文件已被 pane 自删。用 `-L <私有 socket>` 而不是默认 socket,原因有二:
    /// 默认 socket 上 server 多半已在跑(argv 是别人当初那条,测不出东西);以及绝不碰 `soul-*`。
    #[test]
    #[ignore = "需要真实 tmux"]
    fn real_tmux_server_argv_has_no_secret_but_child_env_does() {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let socket = format!("summon-argvproof-{nonce}");
        let session = format!("summon-argvproof-{nonce}");
        let secret = format!("fakesecret{nonce}");
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("launch.env");
        let probe_out = dir.path().join("seen.txt");
        let probe = dir.path().join("probe.sh");
        crate::test_support::write_test_executable(
            &probe,
            "#!/bin/sh\nprintf '%s' \"$FEISHU_APP_SECRET\" > \"$PROBE_OUT\"\nexec /bin/cat\n",
        );

        let pairs = vec![
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            ("FEISHU_APP_SECRET".to_string(), secret.clone()),
            (
                "PROBE_OUT".to_string(),
                probe_out.to_string_lossy().into_owned(),
            ),
        ];
        let (body, skipped) = summon_core::launch_env::render_launch_env_file(&pairs);
        assert!(skipped.is_empty());
        std::fs::write(&env_file, body).unwrap();
        let cmd = summon_core::launch_env::wrap_command_with_env_file(
            &env_file.to_string_lossy(),
            &summon_core::launch_env::sh_single_quote(&probe.to_string_lossy()),
        );

        let argv = Tmux::new(session.clone()).new_session_argv(&dir.path().to_string_lossy(), &cmd);
        let status = Command::new(tmux_bin())
            .args(["-L", &socket])
            .args(&argv)
            .status()
            .unwrap();
        assert!(status.success(), "起私有 socket 会话失败");

        for _ in 0..40 {
            if probe_out.exists() {
                break;
            }
            sleep(Duration::from_millis(50));
        }
        let seen = std::fs::read_to_string(&probe_out).unwrap_or_default();
        let ps = Command::new("ps")
            .args(["-eo", "command"])
            .output()
            .unwrap();
        let ps = String::from_utf8_lossy(&ps.stdout).into_owned();
        let env_file_gone = !env_file.exists();
        // 先无条件拆掉私有 server(只影响本测试自己的 socket),再断言,避免 panic 泄漏进程。
        let _ = Command::new(tmux_bin())
            .args(["-L", &socket, "kill-server"])
            .status();

        // 非空自检:ps 必须真看得见这个私有 server 的命令行,否则下面的"没有 secret"是空断言。
        assert!(
            ps.contains(&socket),
            "ps 里看不到私有 tmux server,断言无意义"
        );
        assert!(
            !ps.contains(&secret),
            "secret 出现在 ps 里(argv 泄漏未修好)"
        );
        assert_eq!(seen, secret, "子进程环境里必须仍拿得到自己的凭据");
        assert!(env_file_gone, "一次性 env 文件应被 pane 载入后自删");
    }

    /// 真机 proof(staging/bridge 路):`env -i` → `sh` → program 是一条 **exec 链**,
    /// 所以 ① 环境从 env 文件送达;② `#{pane_pid}` 仍等于 program 自己的 pid
    /// (`OwnedHandoffStaging::owns_process` 与 bridge 的进程指纹核验全押在这条上);③ argv 无 secret。
    /// 同样跑在私有 `-L` socket 上:不碰默认 socket 的 `soul-*`,也不写 `~/.summon`。
    #[test]
    #[ignore = "需要真实 tmux"]
    fn real_tmux_staging_exec_chain_keeps_pane_pid_and_hides_secret() {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let socket = format!("summon-stgproof-{nonce}");
        let session = format!("summon-bridge-handoff-{}", uuid::Uuid::new_v4());
        let secret = format!("fakesecret{nonce}");
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("launch.env");
        let probe_out = dir.path().join("seen.txt");
        let probe = dir.path().join("probe.sh");
        crate::test_support::write_test_executable(
            &probe,
            "#!/bin/sh\nprintf '%s %s' \"$FEISHU_APP_SECRET\" \"$$\" > \"$PROBE_OUT\"\nexec /bin/cat\n",
        );

        let (body, skipped) = summon_core::launch_env::render_launch_env_file(&[
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            ("FEISHU_APP_SECRET".to_string(), secret.clone()),
            (
                "PROBE_OUT".to_string(),
                probe_out.to_string_lossy().into_owned(),
            ),
        ]);
        assert!(skipped.is_empty());
        std::fs::write(&env_file, body).unwrap();

        let argv = staging_new_session_argv(&session, dir.path(), &env_file, &probe, &[]);
        let status = Command::new(tmux_bin())
            .args(["-L", &socket])
            .args(&argv)
            .status()
            .unwrap();
        assert!(status.success(), "起私有 socket staging 失败");
        for _ in 0..40 {
            if probe_out.exists() {
                break;
            }
            sleep(Duration::from_millis(50));
        }
        let seen = std::fs::read_to_string(&probe_out).unwrap_or_default();
        let pane_pid = Command::new(tmux_bin())
            .args([
                "-L",
                &socket,
                "display-message",
                "-p",
                "-t",
                &format!("={session}:"),
                "-F",
                "#{pane_pid}",
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let ps = Command::new("ps")
            .args(["-eo", "command"])
            .output()
            .unwrap();
        let ps = String::from_utf8_lossy(&ps.stdout).into_owned();
        let _ = Command::new(tmux_bin())
            .args(["-L", &socket, "kill-server"])
            .status();

        assert!(
            ps.contains(&socket),
            "ps 里看不到私有 tmux server,断言无意义"
        );
        assert!(
            !ps.contains(&secret),
            "secret 出现在 ps 里(argv 泄漏未修好)"
        );
        let (got_secret, got_pid) = seen.split_once(' ').expect("probe 没写出结果");
        assert_eq!(
            got_secret, secret,
            "staging 子进程必须拿得到 env 文件里的凭据"
        );
        assert_eq!(
            got_pid, pane_pid,
            "exec 链必须保持 pane_pid = program 自身 pid"
        );
    }

    #[test]
    fn target_forces_exact_match_prefix() {
        // 必须 `=`(精确,防 `soul-x` ⊂ `soul-x-g<tag>` 串台)+ 末尾 `:`(否则 capture-pane/
        // send-keys 的 pane 目标解析不出、报 "can't find pane"、capture 静默返回空)。
        assert_eq!(Tmux::new("soul-starlight").target(), "=soul-starlight:");
        assert_eq!(
            Tmux::new("soul-starlight-g00000000a11ce04c").target(),
            "=soul-starlight-g00000000a11ce04c:"
        );
    }

    #[test]
    #[ignore = "需要真实 tmux server"]
    fn handoff_recovery_treats_missing_exact_session_as_not_launched() {
        let name = format!("{HANDOFF_STAGING_PREFIX}{}", uuid::Uuid::new_v4());
        assert!(!has_session_strict(&name).unwrap());
        assert!(OwnedHandoffStaging::recover_named(&name, &"a".repeat(64))
            .unwrap()
            .is_none());
    }

    #[test]
    fn filter_names_keeps_prefix_only() {
        let all: Vec<String> = [
            "soul-starlight",
            "soul-starlight-pdead",
            "soul-other",
            "misc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let got = filter_names(&all, "soul-starlight");
        assert_eq!(
            got,
            vec![
                "soul-starlight".to_string(),
                "soul-starlight-pdead".to_string()
            ]
        );
    }

    /// 结构守护:CLAUDE.md 第一红线「chat 的回收/空闲扫描线程从不擅杀 tmux」的自动化兜底,替代
    /// inflight.rs 里那个恒真的假 spy 测试。全仓销毁 tmux 会话只有一个封装 `Tmux::kill_session`
    /// (本文件),系统命令字面量 `kill-session` 亦只应在此。扫描整个 summon crate 源码,断言这两个
    /// token 只现身于白名单文件(=五条合法销毁途径的调用点 —— 2026-08-13 新增的飞书 exact
    /// `/quit` **不新增调用点**:它只产出意图,拆除仍落在 `teardown.rs`)。谁在回收/空闲/停机等新位置新增
    /// tmux 销毁 → 本测试变红,逼其显式进白名单复核。
    /// 局限:substring 扫描,注释里写到这两个 token 也会命中——非白名单文件里用描述性措辞、别写原 token。
    /// 换来的是零依赖、跨重构稳定、且不误伤 `ChildKiller::kill`(无头子进程组 SIGKILL,与此正交)。
    #[test]
    fn tmux_kill_session_call_sites_are_whitelisted() {
        use std::collections::BTreeSet;
        // 合法销毁途径的调用点(相对 crates/summon/src)。改这张白名单 = 动第一红线,必须 review。
        const ALLOWED: &[&str] = &[
            "session/tmux.rs",              // 封装本体 + 本守护测试
            "session/session.rs",           // stale-id 清残 + ignored 集成测试清理
            "runtime/daemon/teardown.rs",   // 显式 teardown/kill(合法途径①②)
            "workflow/runner_transient.rs", // 临时 worker 拆除(不碰持久会话)
            "cli/cli.rs",                   // agents down / kill 命令(合法途径①)
            "runtime/daemon/tests.rs", // 仅测试自建会话的 Drop 清理(空闲扫描翻转测试),非生产销毁点
        ];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits: BTreeSet<String> = BTreeSet::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for ent in std::fs::read_dir(&dir).expect("读 src 目录失败") {
                let path = ent.expect("读目录项失败").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let text = std::fs::read_to_string(&path).expect("读源码失败");
                    if text.contains("kill_session") || text.contains("kill-session") {
                        let rel = path
                            .strip_prefix(&src)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/");
                        hits.insert(rel);
                    }
                }
            }
        }
        // 非空自检:遍历必须真读到已知 kill 点(封装本体 + teardown),否则路径/递归坏了会让本测试像
        // 那个假 spy 一样恒绿——这里主动防住「扫描器无声失效」。
        assert!(
            hits.contains("session/tmux.rs") && hits.contains("runtime/daemon/teardown.rs"),
            "扫描没读到已知 kill 点,目录遍历可能坏了;hits={hits:?}"
        );
        let offenders: Vec<&String> = hits
            .iter()
            .filter(|f| !ALLOWED.contains(&f.as_str()))
            .collect();
        assert!(
            offenders.is_empty(),
            "发现未登记的 tmux 销毁调用点(硬规则:回收/空闲扫描线程从不擅杀 tmux)。\
             新增点若合法,加入白名单并 review;否则删掉:{offenders:?}"
        );
    }

    // 需要本机 tmux;默认 ignore,跑:cargo test -p summon -- --ignored
    /// 钉住 `pane_proves_session_gone` 依赖的真机前提:程序自己退出后 ①tmux 会话消亡
    /// ②`capture()` 返回的是 **`Ok(空串)` 而不是 `Err`**(它不检查 tmux 退出码)。
    /// 判据一旦改回「抓屏 Err」就再也不会触发,`/quit` 会退回空转 240s。
    #[test]
    #[ignore = "需要真实 tmux"]
    fn capture_on_dead_session_is_ok_empty_not_err() {
        let t = Tmux::new("summon-test-deadcap");
        let _ = t.kill_session();
        t.new_session(".", "/bin/sh -c 'sleep 1'").unwrap();
        sleep(Duration::from_millis(300));
        assert!(t.has_session(), "程序还在时会话应存在");

        sleep(Duration::from_millis(1500)); // 等程序自己退出
        assert!(!t.has_session(), "程序退出后 tmux 会话应随之消亡");
        let pane = t
            .capture()
            .expect("capture 对死会话仍返回 Ok —— 它不看 tmux 退出码");
        assert!(
            pane.trim().is_empty(),
            "死会话的 capture 应是空串,实得: {pane:?}"
        );
    }

    #[test]
    fn paste_argv_is_bracketed_and_scoped_to_this_session() {
        let t = Tmux::new("soul-starlight-g00000000a11ce04c");
        assert_eq!(
            t.load_buffer_argv(),
            vec![
                "load-buffer",
                "-b",
                "summon-soul-starlight-g00000000a11ce04c",
                "-"
            ],
            "正文必须从 stdin 进 buffer(argv 有长度上限),buffer 名按会话分桶"
        );
        assert_eq!(
            t.paste_buffer_argv(),
            vec![
                "paste-buffer",
                "-b",
                "summon-soul-starlight-g00000000a11ce04c",
                "-t",
                "=soul-starlight-g00000000a11ce04c:",
                "-p",
                "-d"
            ],
            "`-p` 是这次修复的全部要害:没有它就退回逐键 send-keys、大 prompt 丢头部"
        );
    }

    /// 回归:一次注入 3 KB 正文,pane 里的程序必须一个字节不少地收到 —— 尤其是**开头**。
    /// 裸 `send-keys -l` 在 claude TUI 上会吞掉先到的那几块(见 `claude::injects_by_paste`)。
    /// 这里用 `cat` 只守 tmux 这一侧的完整性:每行都短于 canonical 行缓冲上限,排除 tty 自身
    /// 的行长限制,失败就只可能是 paste 通路丢了字节。
    #[test]
    #[ignore]
    fn paste_text_delivers_a_large_payload_head_included() {
        let t = Tmux::new("summon-test-paste-large");
        let _ = t.kill_session();
        t.new_session(".", "cat").unwrap();
        sleep(Duration::from_millis(300));
        let payload: String = (0..60)
            .map(|i| format!("PASTELINE{i:03}-中文正文内容用于撑长这一行的字节数\n"))
            .collect();
        assert!(payload.len() > 3000, "样本要够大才有意义");
        t.paste_text(&payload).unwrap();
        sleep(Duration::from_millis(600));
        let pane = t.capture().unwrap();
        for probe in ["PASTELINE000", "PASTELINE030", "PASTELINE059"] {
            assert!(pane.contains(probe), "{probe} 没到 pane —— 丢字节了");
        }
        t.kill_session().unwrap();
    }

    #[test]
    #[ignore]
    fn roundtrip_with_cat() {
        let t = Tmux::new("summon-test-cat");
        let _ = t.kill_session(); // 清理上次
        t.new_session(".", "cat").unwrap();
        sleep(Duration::from_millis(300));
        t.send_line("hello-summon").unwrap();
        sleep(Duration::from_millis(300));
        let pane = t.capture().unwrap();
        assert!(pane.contains("hello-summon"), "pane was: {pane}");
        t.kill_session().unwrap();
    }

    // 回归:pane 处于 copy-mode(用户滚屏/鼠标选择)时,send-keys 会被吞、prompt 落空。
    // exit_copy_mode 必须先把 pane 退出 copy-mode,后续按键才能真正到达底层程序(此处 cat)。
    #[test]
    #[ignore]
    fn exit_copy_mode_unblocks_send_keys() {
        let t = Tmux::new("summon-test-copymode");
        let _ = t.kill_session();
        t.new_session(".", "cat").unwrap();
        sleep(Duration::from_millis(300));
        // 让 pane 进 copy-mode(模拟用户滚屏)
        Command::new(tmux_bin())
            .args(["copy-mode", "-t", &t.target()])
            .status()
            .unwrap();
        sleep(Duration::from_millis(150));
        // 退出 copy-mode,再发文本,cat 才能收到回显
        t.exit_copy_mode();
        t.send_line("after-copymode").unwrap();
        sleep(Duration::from_millis(300));
        let pane = t.capture().unwrap();
        assert!(
            pane.contains("after-copymode"),
            "退 copy-mode 后仍收不到? pane: {pane}"
        );
        t.kill_session().unwrap();
    }

    // 回归:行首 `-` 的文本必须能字面送达(靠 send_text 的 `--`),否则被 tmux 当 flag 吞掉。
    #[test]
    #[ignore]
    fn send_text_handles_leading_dash() {
        let t = Tmux::new("summon-test-dash");
        let _ = t.kill_session();
        t.new_session(".", "cat").unwrap();
        sleep(Duration::from_millis(300));
        t.send_line("- 列表项 dashline").unwrap();
        sleep(Duration::from_millis(300));
        let pane = t.capture().unwrap();
        assert!(
            pane.contains("dashline"),
            "leading-dash 被吞? pane was: {pane}"
        );
        t.kill_session().unwrap();
    }

    /// staging 的 argv/env/rename/identity cleanup 真机闭环。只清理本测试创建的精确 session，
    /// 并断言所有既有 soul-* 在前后完全一致。
    #[test]
    #[ignore]
    fn handoff_staging_argv_rename_and_cleanup_preserve_soul_sessions() {
        let soul_before = list_sessions_with_prefix("soul-");
        let nonce = uuid::Uuid::new_v4();
        let staging_name = format!("{HANDOFF_STAGING_PREFIX}{nonce}");
        let target = format!("summon-bridge-test-target-{nonce}");
        let marker =
            uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake claude 空格.sh");
        let output = dir.path().join("env output 空格.txt");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\nprintf '%s' \"$BRIDGE_TEST_VALUE\" > \"$BRIDGE_TEST_OUTPUT\"\nexec /bin/cat\n",
        );
        let staging = create_handoff_staging(
            &staging_name,
            &marker,
            dir.path(),
            &script,
            &[],
            &[
                ("BRIDGE_TEST_VALUE".into(), "中文 value with spaces".into()),
                (
                    "BRIDGE_TEST_OUTPUT".into(),
                    output.to_string_lossy().into_owned(),
                ),
            ],
        )
        .unwrap();
        for _ in 0..20 {
            if output.exists() {
                break;
            }
            sleep(Duration::from_millis(50));
        }
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "中文 value with spaces"
        );
        let staging_identity = staging.identity().clone();
        assert!(OwnedHandoffStaging::recover(&staging_identity).is_ok());
        let renamed = staging.rename_exact(&target).unwrap();
        assert_eq!(renamed.identity.name, target);
        assert_eq!(renamed.identity.session_id, staging_identity.session_id);
        assert_eq!(renamed.identity.pane_id, staging_identity.pane_id);
        renamed.cleanup_test_target().unwrap();
        assert_eq!(list_sessions_with_prefix("soul-"), soul_before);
    }

    #[test]
    #[ignore]
    fn handoff_cleanup_rejects_wrong_identity_without_touching_staging() {
        let nonce = uuid::Uuid::new_v4();
        let staging_name = format!("{HANDOFF_STAGING_PREFIX}{nonce}");
        let marker =
            uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
        let staging = create_handoff_staging(
            &staging_name,
            &marker,
            Path::new("/tmp"),
            Path::new("/bin/cat"),
            &[],
            &[],
        )
        .unwrap();
        let mut wrong = staging.identity().clone();
        wrong.session_id = "$999999".into();
        assert!(OwnedHandoffStaging::recover(&wrong).is_err());
        assert!(has_session_strict(&staging_name).unwrap());
        staging.cleanup().unwrap();
    }
}
