//! RPC 常驻会话进程的**跨代登记簿**:`~/.summon/run/rpc-procs.json`。
//!
//! 为什么需要它:`InFlight` 只活在 daemon 进程里。daemon 被 SIGKILL(或 panic 到跑不完
//! 停机钩子)时,那张表连同所有会话句柄一起消失,而 codex `app-server` / cursor `acp` 进程
//! 还在 —— 它们不是 daemon 的前台子进程,没有任何人会去收。一次崩溃留一批,累积无回收。
//!
//! **为什么不能靠 boot 期 `pkill codex`**(sidecar 那条清扫的做法在这里是错的):sidecar 的进程名
//! `summon-sidecar` 是 summon 自己造的,全机只可能是自己的;而 `codex` / `cursor-agent` 是**用户
//! 自己也在用**的 CLI —— 按名字扫等于随时可能把用户手上正跑着的会话打死。所以只能按身份点名:
//! 谁是我们起的,以 spawn 时写下的登记为准。
//!
//! **pid 会被复用**,所以光有 pid 不够。登记同时记下进程的**启动时刻**(`ps -o lstart=`),
//! 清扫时两者都对得上才动手;对不上就是「这个 pid 已经是别人了」,跳过并只清记录。
//! 粒度是秒 —— 同一秒内 pid 恰好被复用且复用者也叫得出同一个 lstart,这个概率不值得
//! 用更重的机制去换,但它是这套判据的已知上界,写在这里免得后人以为它是精确的。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

use crate::observe::log::log;
use crate::runtime::fsio::atomic_write;

/// 登记簿路径。放 `run/`(与 pidfile / sock 同目录):它描述的是**这一代运行时**的进程,
/// 不是配置,也不是历史。
#[cfg(not(test))]
pub fn registry_path() -> PathBuf {
    crate::runtime::paths::run_dir().join("rpc-procs.json")
}

/// 进程测试统一走隔离根，并由 `ProcessTestGuard` 在持有跨进程锁期间清理。绝不能让测试登记
/// 混进真实 `~/.summon/run`，否则下一次 daemon boot 会把夹具当上一代 owner 逐条清扫。
#[cfg(test)]
pub fn registry_path() -> PathBuf {
    test_runtime_root().join("run/rpc-procs.json")
}

/// 本进程持测试闸时用的隔离根,由 `test_support::process_test_guard` 按拿到的槽位设定。
#[cfg(test)]
static TEST_RUNTIME_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_test_runtime_root(root: PathBuf) {
    *TEST_RUNTIME_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(root);
}

#[cfg(test)]
pub(crate) fn clear_test_runtime_root() {
    *TEST_RUNTIME_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[cfg(test)]
pub(crate) fn test_runtime_root() -> PathBuf {
    TEST_RUNTIME_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("summon-rpc-process-tests-runtime"))
}

fn agents_root() -> PathBuf {
    #[cfg(test)]
    {
        test_runtime_root().join("agents")
    }
    #[cfg(not(test))]
    {
        crate::runtime::paths::summon_home().join("agents")
    }
}

/// 读-改-写的串行闸。每 chat 一条线程,冷启/重连会并发落登记。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 一条 RPC 会话进程的登记。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcProc {
    pub pid: i32,
    /// 进程组 id。会话进程 `process_group(0)` 自成一组(leader==child),故与 `pid` 相等;
    /// 分开记是因为**要发信号的是整组**(工具起的孙进程都在里面),语义上不是同一件事。
    pub pgid: i32,
    /// `ps -o lstart=` 的原样输出,防 pid 复用。见模块头。
    pub start: String,
    pub agent: String,
    pub tag: String,
    /// resume_store 的键。spawn 时已知，用来覆盖“strict resume 新进程刚登记、尚未拿到 SID”
    /// 的崩溃窗口。
    #[serde(default)]
    pub session: String,
    /// 协议成功打开/接回会话后 exact 回填。handoff 过程中 tag 可能先于 `.session` move 改变，
    /// SID 才是跨提交点仍稳定的 owner identity。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 进程已证明 Gone，但对应 strict owner 尚未成功消费。这个 durable tombstone 让后续任意
    /// daemon crash 都知道“缺 live registry 是因为旧 writer 已收敛”，而不是登记丢失。
    #[serde(default)]
    pub gone: bool,
}

/// 清扫计划。纯函数产出,便于逐条断言 —— 真去发信号的那一半没有判断。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// 验明正身、要 SIGINT 整组的孤儿。
    pub reap: Vec<RpcProc>,
    /// 已经不在(或 pid 已被别人复用)的记录:只清账,一枪不发。
    pub skip: Vec<RpcProc>,
    /// leader 身份已经无法验证，但进程组仍活着。此时既不能按旧身份发信号（可能误杀复用
    /// pid/pgid 的用户进程），也不能宣称 Gone；只能保留登记并阻断 daemon ready。
    pub blocked: Vec<RpcProc>,
}

/// 按「pid 还在 **且** 启动时刻对得上」验明正身,决定哪些该收。
///
/// `probe` 返回该 pid 当前的启动时刻(`None` = 这个 pid 已经不存在)。抽成注入点是为了让
/// 「pid 复用」这条最难在真机上构造的分支能被确定性地测到 —— 它恰恰是这套机制里唯一
/// 「判错就误杀用户自己进程」的地方。
pub fn plan_sweep(
    records: &[RpcProc],
    probe: &dyn Fn(i32) -> Option<String>,
    group_probe: &dyn Fn(i32) -> bool,
) -> SweepPlan {
    let mut plan = SweepPlan::default();
    for record in records {
        if record.gone {
            plan.skip.push(record.clone());
            continue;
        }
        match probe(record.pid) {
            Some(start) if start == record.start => plan.reap.push(record.clone()),
            _ if group_probe(record.pgid) => plan.blocked.push(record.clone()),
            _ => plan.skip.push(record.clone()),
        }
    }
    plan
}

/// 查一个 pid 的启动时刻。`ps` 而不是平台原生 API:两个目标平台(macOS / Linux)都有,
/// 且这条路径一生只在 spawn 与 boot 各跑一次,不在任何热路径上。
pub fn probe_start(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let start = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!start.is_empty()).then_some(start)
}

fn read_all_checked() -> anyhow::Result<Vec<RpcProc>> {
    let path = registry_path();
    let text = std::fs::read_to_string(&path)
        .with_context(|| summon_i18n::format!("r18aa13bf09", path.display()))?;
    serde_json::from_str(&text).with_context(|| summon_i18n::format!("rdfb065097d", path.display()))
}

/// 运行期 RMW：首次登记允许文件不存在；一旦文件存在，读/parse 错误必须 hard error，绝不能
/// 折成空数组再覆盖，否则会把别的 strict owner 的唯一跨代进程身份静默抹掉。
fn read_all_for_update() -> anyhow::Result<Vec<RpcProc>> {
    let path = registry_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| summon_i18n::format!("rdfb065097d", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => {
            Err(error).with_context(|| summon_i18n::format!("r18aa13bf09", path.display()))
        }
    }
}

/// 在 spawn 任何 RPC 子进程之前先证明跨代登记簿可安全做 RMW。这样 corrupt/unreadable registry
/// 会在还没有新 owner 时失败；不能先 spawn 再发现唯一登记写不进去，只靠本代内存追踪。
pub(crate) fn preflight_recording() -> anyhow::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("radac5174ac")))?;
    read_all_for_update().map(|_| ())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StrictOwner {
    agent: String,
    session: String,
    sid: String,
}

fn strict_owner_matches_record(owner: &StrictOwner, record: &RpcProc) -> bool {
    owner.agent == record.agent
        && (record.session_id.as_deref() == Some(owner.sid.as_str())
            || (record.session == owner.session && record.session_id.is_none()))
}

fn strict_resume_records_at(agents_root: &std::path::Path) -> anyhow::Result<Vec<StrictOwner>> {
    let agents = match std::fs::read_dir(agents_root) {
        Ok(agents) => agents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| summon_i18n::format!("r8e91719885", agents_root.display()))
        }
    };
    let mut owners = Vec::new();
    for agent in agents {
        let agent =
            agent.with_context(|| summon_i18n::format!("re54e75ba05", agents_root.display()))?;
        let agent_dir = agent.path();
        // 与 Registry 的成员判据对齐：只有带 agent.yaml 的条目才是 agent。macOS 会在此目录
        // 自动生成 `.DS_Store`，把普通文件继续拼成 `runtime/chats` 会让全 daemon 无法启动。
        let manifest_path = agent_dir.join("agent.yaml");
        if !manifest_path.is_file() {
            continue;
        }
        // 状态路径走 state_dir 解析(迁移后读新位置,未迁移回退旧位置)
        let alias = agent.file_name().to_string_lossy().to_string();
        let chats = summon_core::registry::agent_state_dir(agents_root, &alias)
            .join("runtime")
            .join("chats");
        let records = match std::fs::read_dir(&chats) {
            Ok(records) => records,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| summon_i18n::format!("r05f2ea8fdf", chats.display()))
            }
        };
        for record in records {
            let path = record
                .with_context(|| summon_i18n::format!("r3ea40467e9", chats.display()))?
                .path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("session") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| summon_i18n::format!("r0323cc5409", path.display()))?;
            anyhow::ensure!(
                metadata.is_file(),
                summon_i18n::format!("r218a050740", path.display())
            );
            let content = std::fs::read_to_string(&path)
                .with_context(|| summon_i18n::format!("rbbf4914497", path.display()))?;
            let parsed = summon_core::session_id::parse_record(&content)
                .with_context(|| summon_i18n::format!("r061b38703f", path.display()))?;
            if parsed.strict_resume {
                let manifest = std::fs::read_to_string(&manifest_path).with_context(|| {
                    summon_i18n::format!("rc5d7775a48", manifest_path.display())
                })?;
                #[derive(Deserialize)]
                struct AgentIdentity {
                    name: String,
                }
                let identity: AgentIdentity =
                    serde_yaml::from_str(&manifest).with_context(|| {
                        summon_i18n::format!("r85942a0721", manifest_path.display())
                    })?;
                let session = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .with_context(|| summon_i18n::format!("r29e3a0436c", path.display()))?;
                owners.push(StrictOwner {
                    agent: identity.name,
                    session: session.to_string(),
                    sid: parsed.id,
                });
            }
        }
    }
    let mut unique = std::collections::HashSet::new();
    for owner in &owners {
        anyhow::ensure!(
            unique.insert((owner.agent.as_str(), owner.sid.as_str())),
            summon_i18n::format!("r904ad8154c", owner.agent, owner.sid)
        );
    }
    Ok(owners)
}

fn read_all_for_sweep(was_crash: bool) -> anyhow::Result<Vec<RpcProc>> {
    let path = registry_path();
    let records: Vec<RpcProc> = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| summon_i18n::format!("rdfb065097d", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => {
            Err(error).with_context(|| summon_i18n::format!("r18aa13bf09", path.display()))
        }
    }?;
    if was_crash {
        let owners = strict_resume_records_at(&agents_root())?;
        for owner in owners {
            anyhow::ensure!(
                records
                    .iter()
                    .any(|record| strict_owner_matches_record(&owner, record)),
                summon_i18n::format!("re32869ddfe", owner.agent, owner.session, owner.sid)
            );
        }
    }
    Ok(records)
}

fn write_all_checked(records: &[RpcProc]) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(records).context(summon_i18n::text!("r238520a97d"))?;
    let path = registry_path();
    atomic_write(&path, &bytes).with_context(|| summon_i18n::format!("r20f79e64a1", path.display()))
}

fn preserve_strict_owner_proofs(
    records: &[RpcProc],
    strict_owners: &[StrictOwner],
    survivors: Vec<RpcProc>,
) -> anyhow::Result<Vec<RpcProc>> {
    let mut persisted = survivors;
    for owner in strict_owners {
        if persisted
            .iter()
            .any(|record| strict_owner_matches_record(owner, record))
        {
            continue;
        }
        let mut tombstone = records
            .iter()
            .find(|record| strict_owner_matches_record(owner, record))
            .cloned()
            .with_context(|| {
                summon_i18n::format!("re887567785", owner.agent, owner.session, owner.sid)
            })?;
        tombstone.gone = true;
        // Gone 记录不再代表一个可发信号的进程身份。归零还能避免多个 owner 的旧 pgid 偶然相同
        // 时被去重逻辑混为一条；后续匹配只认 agent + session/SID。
        tombstone.pid = 0;
        tombstone.pgid = 0;
        tombstone.start.clear();
        persisted.push(tombstone);
    }
    Ok(persisted)
}

/// 纯内存 exact retag。先完成全部身份校验、最后才改字段，失败恒零副作用。
fn retag_records_exact(
    records: &mut [RpcProc],
    agent: &str,
    source_tag: &str,
    target_tag: &str,
    pgid: i32,
) -> anyhow::Result<()> {
    if source_tag == target_tag {
        bail!(summon_i18n::format!("r42c5c6a257"));
    }
    if agent.is_empty() || source_tag.is_empty() || target_tag.is_empty() {
        bail!(summon_i18n::format!("rb7938a1ede"));
    }
    let Some(index) = records.iter().position(|record| record.pgid == pgid) else {
        bail!(summon_i18n::format!("r0cfa5d1216"));
    };
    let record = &records[index];
    if record.pid != pgid || record.agent != agent || record.tag != source_tag {
        bail!(summon_i18n::format!(
            "r1005ee5233",
            record.pid,
            record.agent,
            record.tag
        ));
    }
    records[index].tag = target_tag.to_string();
    Ok(())
}

/// 把一个 summon-owned RPC 进程的跨代登记 exact 搬到新 chat tag。
///
/// 与常规 `record` 的 best-effort 不同，这条路径必须可判错：调用方正持有尚未 commit 的
/// `InFlightRekey`，这里任何读/验身/原子写失败都会让 guard 自动回滚，避免只迁一半。
pub(crate) fn retag_exact(
    agent: &str,
    source_tag: &str,
    target_tag: &str,
    pgid: i32,
) -> anyhow::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r903e10f7bd")))?;
    let mut records = read_all_checked()?;
    retag_records_exact(&mut records, agent, source_tag, target_tag, pgid)?;
    write_all_checked(&records)
}

/// 登记一个刚 spawn 的会话进程。**紧挨着 spawn 调**:这中间 daemon 若被 SIGKILL,
/// 这个进程组就再也没人认得出来了。
pub fn record(agent: &str, tag: &str, session: &str, pid: i32, pgid: i32) -> anyhow::Result<()> {
    let Some(start) = probe_start(pid) else {
        // 查不到启动时刻就不落记录:落一条没法验身的记录,下一代要么不敢动它(等于没记),
        // 要么冒着 pid 复用误杀的风险动它 —— 后者是这套机制唯一不能犯的错。
        log(&summon_i18n::format!("r0c35acd6f0"));
        anyhow::bail!(summon_i18n::format!("r0eacfb085d"));
    };
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rf23fb99448")))?;
    let mut records = read_all_for_update()?;
    records.retain(|r| r.pid != pid);
    records.push(RpcProc {
        pid,
        pgid,
        start,
        agent: agent.to_string(),
        tag: tag.to_string(),
        session: session.to_string(),
        session_id: None,
        gone: false,
    });
    write_all_checked(&records)
}

/// 协议 `thread/start|resume` / `session/new|load` 成功后回填 exact SID。任何身份漂移或登记簿
/// IO 错误都让 checkout 失败；交出一个无法跨代追溯的活会话不属于可接受降级。
pub fn bind_session_exact(
    agent: &str,
    tag: &str,
    session: &str,
    pgid: i32,
    sid: &str,
) -> anyhow::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r685cf720ef")))?;
    let mut records = read_all_checked()?;
    let record = records
        .iter_mut()
        .find(|record| record.pgid == pgid)
        .with_context(|| summon_i18n::format!("r0cfa5d1216"))?;
    anyhow::ensure!(
        record.pid == pgid
            && record.agent == agent
            && record.tag == tag
            && record.session == session,
        summon_i18n::format!("r4bbdb0d379")
    );
    if let Some(existing) = record.session_id.as_deref() {
        anyhow::ensure!(existing == sid, summon_i18n::format!("rf3c89d89b6"));
    }
    record.session_id = Some(sid.to_string());
    record.gone = false;
    records.retain(|candidate| {
        candidate.pgid == pgid
            || !(candidate.gone
                && candidate.agent == agent
                && candidate.session_id.as_deref() == Some(sid))
    });
    write_all_checked(&records)
}

/// 为“boot 已证明旧 writer Gone、但 strict owner 尚未首次 resume”的会话写 durable tombstone。
/// replay slow path 会先写它再标 strict/移动 `.session`：之后任一点再次崩溃，boot 都能区分
/// “登记丢失”与“确实没有 live writer”，而不会为可用性猜测 Gone。
pub(crate) fn record_gone_owner(
    agent: &str,
    tag: &str,
    session: &str,
    sid: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !agent.is_empty() && !tag.is_empty() && !session.is_empty() && !sid.is_empty(),
        summon_i18n::format!("rc3f7251169")
    );
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r44f6c4679c")))?;
    let mut records = read_all_for_update()?;
    upsert_gone_owner(&mut records, agent, tag, session, sid)?;
    write_all_checked(&records)
}

fn upsert_gone_owner(
    records: &mut Vec<RpcProc>,
    agent: &str,
    tag: &str,
    session: &str,
    sid: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !records.iter().any(|record| {
            !record.gone
                && record.agent == agent
                && (record.session == session || record.session_id.as_deref() == Some(sid))
        }),
        summon_i18n::format!("r5ca784fc8f")
    );
    records.retain(|record| {
        !(record.gone
            && record.agent == agent
            && (record.session == session || record.session_id.as_deref() == Some(sid)))
    });
    records.push(RpcProc {
        pid: 0,
        pgid: 0,
        start: String::new(),
        agent: agent.to_string(),
        tag: tag.to_string(),
        session: session.to_string(),
        session_id: Some(sid.to_string()),
        gone: true,
    });
    Ok(())
}

/// 会话已收敛,销账。漏了也不致命(下一代 boot 时 pid 已不在,走 `skip` 分支只清账)。
pub fn forget(pid: i32) {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut records = match read_all_for_update() {
        Ok(records) => records,
        Err(error) => {
            log(&summon_i18n::format!("r7b82ff73db"));
            return;
        }
    };
    let Some(index) = records.iter().position(|record| record.pid == pid) else {
        return;
    };
    let strict_owners = match strict_resume_records_at(&agents_root()) {
        Ok(owners) => owners,
        Err(error) => {
            log(&summon_i18n::format!("r70e06b115b"));
            return;
        }
    };
    let record = &records[index];
    let preserves_strict_owner = strict_owners
        .iter()
        .any(|owner| strict_owner_matches_record(owner, record));
    if preserves_strict_owner {
        records[index].gone = true;
    } else {
        records.remove(index);
    }
    if let Err(error) = write_all_checked(&records) {
        log(&summon_i18n::format!("r37467cb692"));
    }
}

const ORPHAN_SIGINT_GRACE: Duration = Duration::from_secs(2);
const ORPHAN_SIGKILL_GRACE: Duration = Duration::from_secs(2);

fn wait_groups_gone(records: &[RpcProc], grace: Duration) -> Vec<RpcProc> {
    let deadline = Instant::now() + grace;
    loop {
        let alive = records
            .iter()
            .filter(|record| crate::runtime::inflight::group_alive(record.pgid))
            .cloned()
            .collect::<Vec<_>>();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// boot 期清扫上一代 daemon 留下的 RPC 会话进程，并在任何 session 恢复前证明旧进程组 Gone。
///
/// 先给 SIGINT 宽限，让 Codex 写完 `<turn_aborted>`；宽限后仍活着才 SIGKILL，再等一次 Gone。
/// 只有 Gone 的记录才销账。仍无法证明收敛时保留 exact 登记并让 daemon 启动失败：继续 ready 后
/// strict resume 同一 SID 会直接制造第二个 writer，不能拿可用性换这个安全边界。
///
/// 返回成功收敛的旧进程数；登记簿缺失/为空返回 0。
pub fn sweep_orphans(was_crash: bool) -> anyhow::Result<usize> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r9a553aa910")))?;
    let mut records = read_all_for_sweep(was_crash)?;
    let strict_owners = strict_resume_records_at(&agents_root())?;
    if !was_crash {
        // clean shutdown 本身就是“上一代 writer 已 Gone”的证明。旧版本/人工清理没有留下 registry
        // 时，为尚未消费的 strict owner 合成 durable tombstone，避免下一次真正 crash 全局卡死。
        for owner in &strict_owners {
            if records
                .iter()
                .any(|record| strict_owner_matches_record(owner, record))
            {
                continue;
            }
            records.push(RpcProc {
                pid: 0,
                pgid: 0,
                start: String::new(),
                agent: owner.agent.clone(),
                tag: String::new(),
                session: owner.session.clone(),
                session_id: Some(owner.sid.clone()),
                gone: true,
            });
        }
    }
    if records.is_empty() {
        return Ok(0);
    }
    let plan = plan_sweep(
        &records,
        &probe_start,
        &crate::runtime::inflight::group_alive,
    );
    for stale in &plan.skip {
        log(&summon_i18n::format!(
            "rfd7ace4b2d",
            stale.agent,
            stale.pid,
            stale.tag
        ));
    }
    for blocked in &plan.blocked {
        log(&summon_i18n::format!(
            "r27a682d818",
            blocked.agent,
            blocked.pid,
            blocked.pgid,
            blocked.tag
        ));
    }
    for orphan in &plan.reap {
        log(&summon_i18n::format!(
            "r711b142dca",
            orphan.agent,
            orphan.pid,
            orphan.pgid,
            orphan.tag
        ));
        if let Err(error) =
            crate::runtime::rpc_stdio::signal_group(orphan.pgid, nix::sys::signal::Signal::SIGINT)
        {
            log(&summon_i18n::format!("r6ffc939cc6", orphan.pgid));
        }
    }
    let after_sigint = wait_groups_gone(&plan.reap, ORPHAN_SIGINT_GRACE);
    for orphan in &after_sigint {
        log(&summon_i18n::format!(
            "rb526924c3e",
            orphan.agent,
            orphan.pgid,
            orphan.tag
        ));
        if let Err(error) =
            crate::runtime::rpc_stdio::signal_group(orphan.pgid, nix::sys::signal::Signal::SIGKILL)
        {
            log(&summon_i18n::format!("r0180434ef3", orphan.pgid));
        }
    }
    let mut survivors = wait_groups_gone(&after_sigint, ORPHAN_SIGKILL_GRACE);
    survivors.extend(plan.blocked.iter().cloned());
    let persisted = preserve_strict_owner_proofs(&records, &strict_owners, survivors.clone())?;
    write_all_checked(&persisted)?;
    if !survivors.is_empty() {
        let identities = survivors
            .iter()
            .map(|record| format!("{}/{} pgid={}", record.agent, record.tag, record.pgid))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(summon_i18n::format!("rddf2f14ab8"));
    }
    Ok(plan.reap.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_at(pid: i32, start: &str) -> RpcProc {
        RpcProc {
            pid,
            pgid: pid,
            start: start.to_string(),
            agent: "blue".into(),
            tag: "gdeadbeefdeadbeef".into(),
            session: "soul-blue-gdeadbeefdeadbeef".into(),
            session_id: Some(format!("sid-{pid}")),
            gone: false,
        }
    }

    /// 验身两条都要:pid 还在、且启动时刻一致。三种情形各占一条分支。
    #[test]
    fn sweep_plan_reaps_only_processes_that_pass_both_identity_checks() {
        let records = vec![
            proc_at(101, "Wed Aug  5 10:00:00 2026"), // 还在,时刻一致 → 收
            proc_at(102, "Wed Aug  5 10:00:01 2026"), // 已经没了 → 只清账
            proc_at(103, "Wed Aug  5 10:00:02 2026"), // pid 被复用 → 只清账
        ];
        let probe = |pid: i32| match pid {
            101 => Some("Wed Aug  5 10:00:00 2026".to_string()),
            103 => Some("Wed Aug  5 21:33:07 2026".to_string()),
            _ => None,
        };

        let plan = plan_sweep(&records, &probe, &|_| false);
        assert_eq!(
            plan.reap.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![101],
            "只有 pid 存活且启动时刻对得上的才动手"
        );
        assert_eq!(
            plan.skip.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![102, 103],
            "pid 复用必须落进 skip —— 这是唯一会误杀用户自己 codex/cursor 的分支"
        );
        assert!(plan.blocked.is_empty());
    }

    #[test]
    fn sweep_plan_blocks_when_leader_is_gone_but_its_process_group_is_alive() {
        let records = vec![proc_at(101, "Wed Aug  5 10:00:00 2026")];

        let plan = plan_sweep(&records, &|_| None, &|pgid| pgid == 101);

        assert!(plan.reap.is_empty());
        assert!(plan.skip.is_empty());
        assert_eq!(plan.blocked, records);
    }

    /// 空登记簿不产计划(daemon 正常停机后的常态,别在 boot 上白花一次 `ps`)。
    #[test]
    fn sweep_plan_of_an_empty_registry_is_empty() {
        assert_eq!(plan_sweep(&[], &|_| None, &|_| false), SweepPlan::default());
    }

    /// 登记簿是 JSON 数组；坏 JSON 必须显式报错，不能折成空数组再覆盖其它 owner。
    #[test]
    fn records_round_trip_through_json() {
        let records = vec![proc_at(7, "t0"), proc_at(9, "t1")];
        let text = serde_json::to_string(&records).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<RpcProc>>(&text).unwrap(),
            records
        );
        assert!(serde_json::from_str::<Vec<RpcProc>>("{坏的").is_err());
    }

    #[test]
    fn old_registry_records_default_new_owner_fields_without_becoming_gone() {
        let record: RpcProc =
            serde_json::from_str(r#"{"pid":7,"pgid":7,"start":"t0","agent":"blue","tag":"g1"}"#)
                .unwrap();
        assert_eq!(record.session, "");
        assert_eq!(record.session_id, None);
        assert!(!record.gone);
    }

    #[test]
    fn gone_tombstone_is_never_signaled_even_if_pid_and_group_probes_claim_alive() {
        let mut record = proc_at(7, "t0");
        record.gone = true;

        let plan = plan_sweep(&[record.clone()], &|_| Some("t0".into()), &|_| true);

        assert!(plan.reap.is_empty());
        assert!(plan.blocked.is_empty());
        assert_eq!(plan.skip, vec![record]);
    }

    #[test]
    fn gone_owner_upsert_is_exact_and_never_overwrites_a_live_writer() {
        let mut unrelated = proc_at(7, "t0");
        unrelated.agent = "other".into();
        let mut records = vec![unrelated.clone()];

        upsert_gone_owner(&mut records, "blue", "gtarget", "soul-blue-target", "sid-1").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], unrelated);
        let tombstone = &records[1];
        assert!(tombstone.gone);
        assert_eq!(tombstone.pid, 0);
        assert_eq!(tombstone.tag, "gtarget");
        assert_eq!(tombstone.session, "soul-blue-target");
        assert_eq!(tombstone.session_id.as_deref(), Some("sid-1"));

        upsert_gone_owner(&mut records, "blue", "gnew", "soul-blue-new", "sid-1").unwrap();
        assert_eq!(records.len(), 2, "同一 owner 的旧 tombstone 应原位替换");
        assert_eq!(records[1].tag, "gnew");

        let mut live = proc_at(9, "t1");
        live.session_id = Some("sid-live".into());
        records.push(live);
        let before = records.clone();
        assert!(upsert_gone_owner(
            &mut records,
            "blue",
            "gtarget",
            "soul-blue-target",
            "sid-live"
        )
        .is_err());
        assert_eq!(records, before, "live 冲突时必须零副作用");
    }

    #[test]
    fn sweep_preserves_one_gone_proof_for_each_strict_owner() {
        let first_owner = StrictOwner {
            agent: "blue".into(),
            session: "soul-blue-g1".into(),
            sid: "sid-1".into(),
        };
        let second_owner = StrictOwner {
            agent: "blue".into(),
            session: "soul-blue-g2".into(),
            sid: "sid-2".into(),
        };
        let mut first = proc_at(7, "t0");
        first.session = first_owner.session.clone();
        first.session_id = Some(first_owner.sid.clone());
        let mut second = proc_at(9, "t1");
        second.session = second_owner.session.clone();
        second.session_id = Some(second_owner.sid.clone());

        let persisted = preserve_strict_owner_proofs(
            &[first, second],
            &[first_owner.clone(), second_owner.clone()],
            Vec::new(),
        )
        .unwrap();

        assert_eq!(persisted.len(), 2, "pgid=0 的 tombstone 也不能互相吞掉");
        assert!(persisted.iter().all(|record| record.gone));
        assert!(persisted
            .iter()
            .any(|record| strict_owner_matches_record(&first_owner, record)));
        assert!(persisted
            .iter()
            .any(|record| strict_owner_matches_record(&second_owner, record)));
    }

    #[test]
    fn exact_retag_changes_only_the_matching_process() {
        let mut records = vec![proc_at(7, "t0"), proc_at(9, "t1")];
        retag_records_exact(&mut records, "blue", "gdeadbeefdeadbeef", "gtarget", 7).unwrap();

        assert_eq!(records[0].tag, "gtarget");
        assert_eq!(records[1], proc_at(9, "t1"), "其它进程登记不得被连带修改");
    }

    #[test]
    fn exact_retag_mismatch_has_no_side_effect() {
        let original = vec![proc_at(7, "t0"), proc_at(9, "t1")];
        for (agent, source, pgid) in [
            ("red", "gdeadbeefdeadbeef", 7),
            ("blue", "gwrong", 7),
            ("blue", "gdeadbeefdeadbeef", 99),
        ] {
            let mut records = original.clone();
            assert!(retag_records_exact(&mut records, agent, source, "gtarget", pgid).is_err());
            assert_eq!(records, original, "exact identity 不匹配时必须零副作用");
        }
    }

    #[test]
    fn strict_resume_scan_is_checked_and_detects_handoff_owners() {
        let tmp = tempfile::tempdir().unwrap();
        let chats = tmp.path().join("blue/runtime/chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(tmp.path().join("blue/agent.yaml"), "name: blue\n").unwrap();
        std::fs::write(
            chats.join("soul-blue-g1.session"),
            summon_core::session_id::format_record("codex", "th-1", None, None),
        )
        .unwrap();
        assert!(strict_resume_records_at(tmp.path()).unwrap().is_empty());

        std::fs::write(
            chats.join("soul-blue-g2.session"),
            summon_core::session_id::format_strict_record(
                "cursor-acp",
                "sess-1",
                Some("/work"),
                None,
            ),
        )
        .unwrap();
        assert_eq!(
            strict_resume_records_at(tmp.path()).unwrap(),
            vec![StrictOwner {
                agent: "blue".into(),
                session: "soul-blue-g2".into(),
                sid: "sess-1".into(),
            }]
        );

        std::fs::remove_file(chats.join("soul-blue-g2.session")).unwrap();
        std::fs::write(chats.join("soul-blue-g3.session"), "half-written").unwrap();
        assert!(
            strict_resume_records_at(tmp.path()).is_err(),
            "损坏记录不能被折成‘没有 strict owner’"
        );
    }

    #[test]
    fn strict_resume_scan_ignores_non_agent_entries_but_keeps_real_owners() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".DS_Store"), b"finder metadata").unwrap();
        let stray_chats = tmp.path().join("not-an-agent/runtime/chats");
        std::fs::create_dir_all(&stray_chats).unwrap();
        std::fs::write(stray_chats.join("broken.session"), b"half-written").unwrap();

        let chats = tmp.path().join("blue/runtime/chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(tmp.path().join("blue/agent.yaml"), "name: blue\n").unwrap();
        std::fs::write(
            chats.join("soul-blue-g1.session"),
            summon_core::session_id::format_strict_record("codex", "th-1", None, None),
        )
        .unwrap();

        assert_eq!(
            strict_resume_records_at(tmp.path()).unwrap(),
            vec![StrictOwner {
                agent: "blue".into(),
                session: "soul-blue-g1".into(),
                sid: "th-1".into(),
            }]
        );
    }

    #[test]
    fn test_registry_path_is_never_the_real_runtime_registry() {
        assert_ne!(
            registry_path(),
            crate::runtime::paths::run_dir().join("rpc-procs.json")
        );
        assert!(registry_path().starts_with(std::env::temp_dir()));
    }

    /// `probe_start` 对自己这个进程必须查得到,对一个几乎不可能存在的 pid 必须查不到。
    /// (这条把「`ps -o lstart=` 在本平台可用」钉住 —— 它不可用的话整套验身会静默退化成
    /// 「一条都不收」,而那种退化没有任何外部症状。)
    #[test]
    fn probe_start_sees_this_process_and_not_a_bogus_pid() {
        let me = std::process::id() as i32;
        assert!(
            probe_start(me).is_some_and(|s| !s.is_empty()),
            "ps -o lstart= 在本平台不可用,孤儿验身会静默退化成一条都不收"
        );
        assert_eq!(probe_start(999_999), None);
    }
}
