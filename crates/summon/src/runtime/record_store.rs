//! cassette 落盘/读回 IO(录制-回放夹具)。**整模块 `#[cfg(debug_assertions)]` gate** —— release
//! 构建里不编译,生产二进制零录制代码(同 `summon dev`)。
//!
//! 布局(`<root>/<scenario>/turns/turn-<n>/`):`spawn.json`(`CassetteTurn` serde)+ `stdout.raw`
//! (子进程 stdout 原始字节,逐字节忠实)。`root` 注入(默认 `summon_home()/raw`),便于测用 tempdir。
//!
//! 这里只做 IO(纯数据形状/白名单在 `summon_core::record`);写入由 debug 构建里的 `RecordingExecutor`
//! (录 spawn)+ `Subprocess` 读循环(tee stdout 原始字节)分两点调用。

#![cfg(debug_assertions)]
// 写入路径(start_turn/write_spawn/append_ndjson/snapshot_session)生产侧已接
// (daemon `record_*`);读回路径(load_turn/read_ndjson)主要供 `#[cfg(test)]` 回放与 `dev record
// promote`。少数读 API 仅测试用,就近 `#[allow(dead_code)]` 标注,不再整模块压制。

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde::Serialize;
use summon_core::record::{claude_transcript_matches_sid, codex_rollout_matches_sid, CassetteTurn};

/// 场景 ndjson(inbound/outbound/timeline)单文件上限。录制是 append-only 且没有任何清理:实测
/// `raw/baseline/outbound.ndjson` 44 天涨到 45MB(StreamPatchCard 全量 card_json 约 6.2KB/行)。
/// 取值与 `CHAT_LOG_MAX_BYTES` 一致(50MB):同为「无界 append 的观测/夹具流」,没有理由各定一套。
/// 超这个量的 ndjson 也早不适合当回放素材了——保留一代 `.1` 够排查,再往前的丢掉。
const NDJSON_MAX_BYTES: u64 = crate::observe::event_log::CHAT_LOG_MAX_BYTES;

/// 会话快照按 CLI 分桶的子目录名(`<scenario>/session/<cli>/<file>`)。回放挂载(RR3.2)把临时
/// HOME 指向重建出的 `~/.claude`/`~/.codex` 等,解析层据此读到这一轮的会话文件、不碰真实家目录。
const SESSION_DIR: &str = "session";

/// 录制会话快照时,识别 CLI 家族用的最小枚举(避免把 summon-core 的 `CliKind`/`CliDriver` 拖进
/// 纯 IO 层)。调用方(daemon 录制点)按 agent 的 cli_kind 映射成这三类之一。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCli {
    /// claude:`~/.claude/projects/<project_dirname(cwd)>/<sid>.jsonl`,cwd+sid 确定性定位。
    Claude,
    /// codex:`~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<sid>.jsonl`,扫描文件名含 sid。
    Codex,
    /// traex:codex fork,rollout 同构,根在 `~/.trae/cli/sessions/`(多一层 `cli/`)。
    Traex,
    /// cursor:`~/.cursor/chats/<chat-hex>/…`,无 sid→路径确定映射,录制侧暂不快照(回放退化为
    /// 转录解析层或 stdout 级回放足够;cursor resume 文件按需在 owner 真录时补)。
    Cursor,
}

/// cassette 仓:持 raw 根目录,提供「起一轮 → 写 spawn / 拿 stdout 写句柄 → 读回」的最小面。
pub struct RecordStore {
    root: PathBuf,
}

impl RecordStore {
    /// 注入 raw 根(测用 tempdir;生产默认 `summon_home()/raw`,由调用方传入)。    // test-only:turn 录制的写入侧。生产接缝(`RecordingExecutor`)已收进
    // `cfg(all(test, debug_assertions))`,只有回放保真测还在构造它。
    #[cfg(test)]
    /// 起第 `n` 轮:建 `<root>/<scenario>/turns/turn-<n>/` 并返回该目录(后续写 spawn.json/stdout.raw)。
    pub fn start_turn(&self, scenario: &str, n: usize) -> anyhow::Result<PathBuf> {
        let dir = self.turn_dir(scenario, n);
        fs::create_dir_all(&dir)
            .with_context(|| summon_i18n::format!("r437c51ec5c", dir.display()))?;
        Ok(dir)
    }

    // test-only:turn 录制的写入侧。生产接缝(`RecordingExecutor`)已收进
    // `cfg(all(test, debug_assertions))`,只有回放保真测还在构造它。
    #[cfg(test)]
    /// 落 `spawn.json`(`CassetteTurn` serde)到 turn 目录。
    pub fn write_spawn(&self, turn_dir: &Path, turn: &CassetteTurn) -> anyhow::Result<()> {
        let path = turn_dir.join("spawn.json");
        let json = serde_json::to_string_pretty(turn).context(summon_i18n::text!("rb0b7388b05"))?;
        fs::write(&path, json)
            .with_context(|| summon_i18n::format!("r989de1fa2f", path.display()))?;
        Ok(())
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 读回某一轮的 `CassetteTurn`(回放/校验用)。仅 `#[cfg(test)]` 回放测断言用
    /// (promote 经 `infer_cli` 直接读 spawn.json,生产录制只写不读)。
    #[allow(dead_code)]
    pub fn load_turn(&self, scenario: &str, n: usize) -> anyhow::Result<CassetteTurn> {
        let path = self.turn_dir(scenario, n).join("spawn.json");
        let json = fs::read_to_string(&path)
            .with_context(|| summon_i18n::format!("re910f7bd5e", path.display()))?;
        serde_json::from_str(&json)
            .with_context(|| summon_i18n::format!("rc5bf0184c1", path.display()))
    }

    /// 场景根目录 `<root>/<scenario>/`(`inbound.ndjson`/`outbound.ndjson`/`scenario.json` 落这里;
    /// turn 子目录在其下)。append 前由调用方建好。
    pub fn scenario_dir(&self, scenario: &str) -> PathBuf {
        self.root.join(scenario)
    }

    /// 往场景根下 `<file>`(如 `inbound.ndjson`/`outbound.ndjson`)追加一条 serde 记录(一行 JSON)。
    /// 建目录幂等;append 模式,按到达序累积。入站 tee(RR2.2)/出站录(RR2.3)共用。
    /// 打开前先按 `NDJSON_MAX_BYTES` 轮转(复用 events.jsonl 那套:超限 rename 成 `.1`,只留一代),
    /// 否则录制会无上限吃盘。轮转本身失败就继续 append——宁可文件偏大,不能因此丢录制。
    pub fn append_ndjson<T: Serialize>(
        &self,
        scenario: &str,
        file: &str,
        record: &T,
    ) -> anyhow::Result<()> {
        let dir = self.scenario_dir(scenario);
        fs::create_dir_all(&dir)
            .with_context(|| summon_i18n::format!("r105b54f908", dir.display()))?;
        let path = dir.join(file);
        crate::observe::event_log::rotate_if_oversized(&path, NDJSON_MAX_BYTES);
        let line =
            serde_json::to_string(record).with_context(|| summon_i18n::format!("rbe99c5984f"))?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| summon_i18n::format!("rc81dd3edc0", path.display()))?;
        f.write_all(line.as_bytes())
            .and_then(|_| f.write_all(b"\n"))
            .with_context(|| summon_i18n::format!("r03fb63021e", path.display()))?;
        Ok(())
    }

    /// 读回场景根下 `<file>` 的全部 ndjson 记录(按行反序列化)。空行跳过;任一行坏即 Err。
    /// 文件不存在返回空 Vec(场景内该侧无记录是合法的)。
    pub fn read_ndjson<T: DeserializeOwned>(
        &self,
        scenario: &str,
        file: &str,
    ) -> anyhow::Result<Vec<T>> {
        let path = self.scenario_dir(scenario).join(file);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| summon_i18n::format!("rb1a59430f1", path.display()))
            }
        };
        let mut out = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let rec = serde_json::from_str(line)
                .with_context(|| summon_i18n::format!("rdea618b4b1", path.display(), i + 1))?;
            out.push(rec);
        }
        Ok(out)
    }

    /// 会话快照根:`<root>/<scenario>/session/`(回放挂载把临时 HOME 子树指向这里)。
    pub fn session_dir(&self, scenario: &str) -> PathBuf {
        self.scenario_dir(scenario).join(SESSION_DIR)
    }

    /// RR3.1 会话快照:按 CLI **只拷该轮真读写的会话文件**进 `<scenario>/session/<cli>/`,绝不整盘
    /// 拷家目录。`home` 注入(测用 tempdir;生产传 `dirs::home_dir()`),`session_id` 为本轮 sid,
    /// `work_dir` 仅 claude 用(确定性拼项目目录)。返回拷贝的文件数(0=没找到匹配,合法:cursor 或
    /// sid 尚未落盘)。**best-effort**:单个文件拷失败只计入错误日志由调用方处理,这里整体 Result。
    ///
    /// - claude:`home/.claude/projects/<project_dirname(work_dir)>/<sid>.jsonl` 确定性定位,存在即拷。
    /// - codex:扫 `home/.codex/sessions/`(递归)取文件名含 sid 的 `rollout-*.jsonl`,逐个拷。
    /// - traex:codex fork,同构扫 `home/.trae/cli/sessions/`(多一层 `cli/`)。
    /// - cursor:无 sid→路径映射,跳过(返回 0)。
    pub fn snapshot_session(
        &self,
        scenario: &str,
        cli: SessionCli,
        session_id: &str,
        work_dir: Option<&str>,
        home: &Path,
    ) -> anyhow::Result<usize> {
        if session_id.is_empty() {
            return Ok(0); // 无 sid 不快照(避免误拷)
        }
        match cli {
            SessionCli::Claude => self.snapshot_claude(scenario, session_id, work_dir, home),
            SessionCli::Codex => {
                self.snapshot_codex_family(scenario, session_id, home, ".codex/sessions", "codex")
            }
            SessionCli::Traex => self.snapshot_codex_family(
                scenario,
                session_id,
                home,
                ".trae/cli/sessions",
                "traex",
            ),
            SessionCli::Cursor => Ok(0),
        }
    }

    /// claude:确定性路径 `home/.claude/projects/<project_dirname(cwd)>/<sid>.jsonl`,存在即拷进
    /// `session/claude/<sid>.jsonl`(保原文件名,回放挂载时 `claude_transcript_path` 能原样命中)。
    fn snapshot_claude(
        &self,
        scenario: &str,
        session_id: &str,
        work_dir: Option<&str>,
        home: &Path,
    ) -> anyhow::Result<usize> {
        let Some(cwd) = work_dir else {
            return Ok(0); // 无 cwd 无法定位 claude 转录
        };
        let proj = summon_core::claude_log::project_dirname(cwd);
        let src = home
            .join(".claude")
            .join("projects")
            .join(&proj)
            .join(format!("{session_id}.jsonl"));
        if !src.is_file() {
            return Ok(0);
        }
        // 防御:文件名须确属该 sid(确定性拼出的,理应恒真;断言以防上游传错)。
        let fname = format!("{session_id}.jsonl");
        if !claude_transcript_matches_sid(&fname, session_id) {
            return Ok(0);
        }
        // 重建相对子树 `session/claude/projects/<proj>/<sid>.jsonl`:回放把临时 HOME 指向
        // `session/claude` 时,`<HOME>/.claude/projects/...` 需对应——故快照里同样以 `.claude/` 起。
        let dst = self
            .session_dir(scenario)
            .join("claude")
            .join(".claude")
            .join("projects")
            .join(&proj)
            .join(&fname);
        copy_file(&src, &dst)?;
        Ok(1)
    }

    /// codex 系(codex/traex):递归扫 `<home>/<root_subdir>/`,文件名含 sid 的 `rollout-*.jsonl`
    /// 全拷进 `session/<family>/<原相对 home 的路径>`(保日期分桶,回放把 HOME 指向 `session/<family>`)。
    /// 两 CLI 仅根子目录与家族名不同(codex=`.codex/sessions`,traex=`.trae/cli/sessions`)。
    fn snapshot_codex_family(
        &self,
        scenario: &str,
        session_id: &str,
        home: &Path,
        root_subdir: &str,
        family: &str,
    ) -> anyhow::Result<usize> {
        let sessions_root = home.join(root_subdir);
        if !sessions_root.is_dir() {
            return Ok(0);
        }
        let mut copied = 0usize;
        for src in walk_files(&sessions_root)? {
            let fname = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !codex_rollout_matches_sid(&fname, session_id) {
                continue;
            }
            // 保留相对 home 的子路径(`.codex/sessions/<YYYY>/<MM>/<DD>/rollout-*.jsonl`)。
            let rel = src.strip_prefix(home).unwrap_or(&src);
            let dst = self.session_dir(scenario).join(family).join(rel);
            copy_file(&src, &dst)?;
            copied += 1;
        }
        Ok(copied)
    }

    fn turn_dir(&self, scenario: &str, n: usize) -> PathBuf {
        self.root
            .join(scenario)
            .join("turns")
            .join(format!("turn-{n}"))
    }
}

/// 拷一个文件到 `dst`(建好父目录)。回放需逐字节忠实,故用 `fs::copy`(保内容/权限)。
fn copy_file(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| summon_i18n::format!("r3c96793d73", parent.display()))?;
    }
    fs::copy(src, dst)
        .with_context(|| summon_i18n::format!("rcea0bc0f5f", src.display(), dst.display()))?;
    Ok(())
}

/// 递归列出目录下所有普通文件(会话快照按需扫描用)。深度无限但树小(codex sessions 按日期分桶)。
fn walk_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| summon_i18n::format!("r2ae27a781a", dir.display()))
            }
        };
        for ent in rd {
            let ent = ent.with_context(|| summon_i18n::format!("r745ce96230", dir.display()))?;
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use summon_core::record::SpawnSpecRecord;

    fn sample_turn() -> CassetteTurn {
        CassetteTurn {
            spec: SpawnSpecRecord {
                program: "codex".into(),
                args: vec!["exec".into(), "--json".into()],
                work_dir: Some("/tmp/wd".into()),
                stdin_null: true,
                env: vec![("SUMMON_CHAT_ID".into(), "oc_1".into())],
            },
            stdout_file: "stdout.raw".into(),
        }
    }

    #[test]
    fn write_then_load_turn_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecordStore::new(tmp.path());
        let turns = [
            sample_turn(),
            CassetteTurn {
                spec: SpawnSpecRecord {
                    program: "cursor-agent".into(),
                    args: vec!["-p".into()],
                    work_dir: None,
                    stdin_null: false,
                    env: vec![],
                },
                stdout_file: "stdout.raw".into(),
            },
        ];
        for (index, turn) in turns.into_iter().enumerate() {
            let dir = store.start_turn("scn1", index).unwrap();
            assert!(dir.is_dir(), "start_turn 应建出 turn 目录");
            store.write_spawn(&dir, &turn).unwrap();
            assert_eq!(store.load_turn("scn1", index).unwrap(), turn);
        }
    }

    #[test]
    fn load_missing_turn_errors_not_panics() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecordStore::new(tmp.path());
        assert!(store.load_turn("nope", 9).is_err());
    }

    /// RR3.1 claude:确定性路径下的 `<sid>.jsonl` 被拷进 `session/claude/.claude/projects/...`,
    /// 且内容逐字节一致;邻轮(别的 sid)不被误拷。
    #[test]
    fn snapshot_claude_copies_only_pinned_sid() {
        let home = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        let store = RecordStore::new(raw.path());
        let cwd = "/Users/me/work";
        let proj = summon_core::claude_log::project_dirname(cwd);
        let proj_dir = home.path().join(".claude").join("projects").join(&proj);
        fs::create_dir_all(&proj_dir).unwrap();
        let sid = "07778859-d337-4a01-a41d-e4c11ae7be81";
        fs::write(
            proj_dir.join(format!("{sid}.jsonl")),
            b"{\"t\":\"claude\"}\n",
        )
        .unwrap();
        // 邻轮(别的 sid 同目录):不应被拷。
        fs::write(proj_dir.join("other-sid.jsonl"), b"{}\n").unwrap();

        let n = store
            .snapshot_session("scn", SessionCli::Claude, sid, Some(cwd), home.path())
            .unwrap();
        assert_eq!(n, 1, "只拷钉死 sid 的那一份");
        let snapped = store
            .session_dir("scn")
            .join("claude")
            .join(".claude")
            .join("projects")
            .join(&proj)
            .join(format!("{sid}.jsonl"));
        assert_eq!(fs::read(&snapped).unwrap(), b"{\"t\":\"claude\"}\n");
        // 邻轮没被带进来。
        assert!(!store
            .session_dir("scn")
            .join("claude/.claude/projects")
            .join(&proj)
            .join("other-sid.jsonl")
            .exists());
    }

    /// RR3.1 codex:扫日期分桶下文件名含 sid 的 rollout,保相对子树拷进 `session/codex/.codex/...`。
    #[test]
    fn snapshot_codex_copies_matching_rollout_preserving_tree() {
        let home = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        let store = RecordStore::new(raw.path());
        let sid = "019e6224-f982-7cb0-a2ea-6701ba379e31";
        let day = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("26");
        fs::create_dir_all(&day).unwrap();
        let fname = format!("rollout-2026-05-26T10-37-30-{sid}.jsonl");
        fs::write(day.join(&fname), b"{\"codex\":1}\n").unwrap();
        // 别的会话同桶:不命中。
        fs::write(
            day.join("rollout-2026-05-26T11-00-00-deadbeef-0000.jsonl"),
            b"{}\n",
        )
        .unwrap();

        let n = store
            .snapshot_session("scn", SessionCli::Codex, sid, None, home.path())
            .unwrap();
        assert_eq!(n, 1);
        let snapped = store
            .session_dir("scn")
            .join("codex")
            .join(".codex/sessions/2026/05/26")
            .join(&fname);
        assert_eq!(fs::read(&snapped).unwrap(), b"{\"codex\":1}\n");
    }

    /// RR3.1 traex:同构扫 `~/.trae/cli/sessions/`(比 codex 多一层 `cli/`),保相对子树拷进
    /// `session/traex/.trae/cli/sessions/...`。
    #[test]
    fn snapshot_traex_copies_matching_rollout_preserving_tree() {
        let home = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        let store = RecordStore::new(raw.path());
        let sid = "019e6224-f982-7cb0-a2ea-6701ba379e31";
        let day = home
            .path()
            .join(".trae")
            .join("cli")
            .join("sessions")
            .join("2026")
            .join("08")
            .join("18");
        fs::create_dir_all(&day).unwrap();
        let fname = format!("rollout-2026-08-18T10-37-30-{sid}.jsonl");
        fs::write(day.join(&fname), b"{\"traex\":1}\n").unwrap();
        // 别的会话同桶:不命中。
        fs::write(
            day.join("rollout-2026-08-18T11-00-00-deadbeef-0000.jsonl"),
            b"{}\n",
        )
        .unwrap();

        let n = store
            .snapshot_session("scn", SessionCli::Traex, sid, None, home.path())
            .unwrap();
        assert_eq!(n, 1);
        let snapped = store
            .session_dir("scn")
            .join("traex")
            .join(".trae/cli/sessions/2026/08/18")
            .join(&fname);
        assert_eq!(fs::read(&snapped).unwrap(), b"{\"traex\":1}\n");
    }

    /// 超上限的 ndjson 在下一次 append 前被轮转成 `.1`,新文件从头开始只含新写的那一行。
    #[test]
    fn append_ndjson_rotates_oversized_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecordStore::new(tmp.path());
        let dir = store.scenario_dir("scn");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("outbound.ndjson");
        // 用稀疏文件撑到超限(不真写 50MB,测跑得快;rotate 只看 metadata.len)。
        let f = File::create(&path).unwrap();
        f.set_len(NDJSON_MAX_BYTES + 1).unwrap();
        drop(f);

        store
            .append_ndjson("scn", "outbound.ndjson", &serde_json::json!({"k": 1}))
            .unwrap();

        assert!(dir.join("outbound.ndjson.1").is_file(), "上一代保留为 .1");
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 1, "轮转后新文件从头开始");
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            content.len() as u64,
            "新文件不该继承旧文件的体积"
        );
    }

    /// 未超限不轮转:连续 append 累积在同一个文件里,不产生 `.1`。
    #[test]
    fn append_ndjson_keeps_appending_below_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecordStore::new(tmp.path());
        for i in 0..3 {
            store
                .append_ndjson("scn", "inbound.ndjson", &serde_json::json!({ "i": i }))
                .unwrap();
        }
        let dir = store.scenario_dir("scn");
        assert!(!dir.join("inbound.ndjson.1").exists(), "未超限不应轮转");
        let back: Vec<serde_json::Value> = store.read_ndjson("scn", "inbound.ndjson").unwrap();
        assert_eq!(back.len(), 3);
    }

    /// cursor 暂不快照(无 sid→路径确定映射);空 sid 任何 CLI 都不拷。
    #[test]
    fn snapshot_cursor_and_empty_sid_copy_nothing() {
        let home = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        let store = RecordStore::new(raw.path());
        assert_eq!(
            store
                .snapshot_session("scn", SessionCli::Cursor, "sid-x", None, home.path())
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .snapshot_session("scn", SessionCli::Claude, "", Some("/w"), home.path())
                .unwrap(),
            0
        );
    }
}
