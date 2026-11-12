//! 备份与清理的两阶段接口。压缩包按已鉴权请求分块下载，确认前须完整下载并确认保存。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use summon_core::dashboard::authz::{self, AgentLists};
use summon_core::registry::Agent;

use super::{internal, params, ApiEnv};
use crate::runtime::dashboard::agent_archive::{self, Backup};
use crate::runtime::dashboard::server::{ApiCtx, ApiError, ErrorCode};
use crate::runtime::dashboard::sessions::Principal;

pub(crate) struct Job {
    agent: String,
    principal: Principal,
    created: Instant,
    state: Mutex<State>,
}
enum State {
    Preparing,
    Ready {
        backup: Backup,
        downloaded: u64,
    },
    /// 备份已交给清理 worker，状态接口不再暴露可下载的 owner。
    Cleaning,
    Done {
        message: Option<String>,
    },
    PreparationFailed {
        message: String,
    },
    /// 只有完整下载并确认之后才会清理；失败保留压缩包，但不重新开放下载或清理。
    CleanupFailed {
        message: String,
        backup: Backup,
    },
}

impl State {
    fn status(&self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Ready { .. } => "ready",
            Self::Cleaning => "cleaning",
            Self::Done { .. } => "done",
            Self::PreparationFailed { .. } | Self::CleanupFailed { .. } => "failed",
        }
    }

    fn busy(&self) -> bool {
        matches!(self, Self::Preparing | Self::Cleaning)
    }

    fn message(&self) -> Option<&str> {
        match self {
            Self::Done { message } => message.as_deref(),
            Self::PreparationFailed { message } | Self::CleanupFailed { message, .. } => {
                Some(message)
            }
            _ => None,
        }
    }

    fn backup(&self) -> Option<&Backup> {
        match self {
            Self::Ready { backup, .. } | Self::CleanupFailed { backup, .. } => Some(backup),
            _ => None,
        }
    }

    fn downloaded(&self) -> bool {
        match self {
            Self::Ready { backup, downloaded } => *downloaded == backup.bytes,
            Self::CleanupFailed { .. } => true,
            _ => false,
        }
    }
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn denied(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Forbidden, message)
}

type Worker = Box<dyn FnOnce() + Send + 'static>;

fn spawn(name: &str, worker: Worker) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(worker)
        .map(|_| ())
}

/// 压缩包含凭据与完整会话，只认明确名单或应用所有者，不能使用空名单私聊的便捷放行。
fn allowed(env: &ApiEnv, principal: &Principal, alias: &str) -> Result<Agent, ApiError> {
    let agent = env.agent(alias)?;
    if let Principal::Feishu(issuer) = principal {
        let open = if issuer.agent == alias {
            issuer.open_id.as_str()
        } else {
            ""
        };
        authz::explicitly_authorized(
            &AgentLists::from_agent(&agent, false),
            open,
            &issuer.union_id,
        )
        .map_err(denied)?;
    }
    Ok(agent)
}
fn job(ctx: &ApiCtx, id: &str) -> Result<Arc<Job>, ApiError> {
    let mut jobs = lock(&ctx.env()?.cleanup);
    jobs.retain(|_, j| j.created.elapsed() < Duration::from_secs(86400) || lock(&j.state).busy());
    let job = jobs
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, summon_i18n::text!("r47aa3a27d6")))?;
    drop(jobs);
    if job.principal != ctx.principal {
        return Err(denied(summon_i18n::text!("rd02b3f3caa")));
    }
    // 完成后原 agent 已不存在，结果仅向创建者返回；没有备份字节可取。
    if !matches!(*lock(&job.state), State::Cleaning | State::Done { .. }) {
        allowed(ctx.env()?, &ctx.principal, &job.agent)?;
    }
    Ok(job)
}
fn view(id: &str, job: &Job) -> Value {
    let s = lock(&job.state);
    view_state(id, job, &s)
}

fn view_state(id: &str, job: &Job, state: &State) -> Value {
    let backup = state.backup();
    json!({"job":id, "agent":job.agent, "status":state.status(), "message":state.message(),
        "bytes":backup.map(|b|b.bytes), "sha256":backup.map(|b|&b.sha256),
        "filename":format!("{}-backup.tar.gz",job.agent),
        "paths":backup.map(|b|&b.plan.roots),
        "downloaded":state.downloaded()})
}
#[derive(Deserialize)]
struct AgentParam {
    agent: String,
}
#[derive(Deserialize)]
struct JobParam {
    job: String,
}

pub(crate) fn prepare(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    prepare_with_spawn(ctx, p, spawn)
}

fn prepare_with_spawn(
    ctx: &ApiCtx,
    p: Value,
    spawn: impl FnOnce(&str, Worker) -> std::io::Result<()>,
) -> Result<Value, ApiError> {
    let AgentParam { agent: alias } = params(p)?;
    let env = ctx.env_arc()?;
    let agent = allowed(&env, &ctx.principal, &alias)?;
    if matches!(&ctx.principal, Principal::Feishu(issuer) if issuer.agent == alias) {
        return Err(denied(summon_i18n::text!("reca15d289b")));
    }
    if env.enabled(&alias) {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            summon_i18n::text!("r2dbe8650d3"),
        ));
    }
    env.port
        .agent_idle(&agent)
        .map_err(|m| ApiError::new(ErrorCode::Busy, m))?;
    let mut jobs = lock(&env.cleanup);
    jobs.retain(|_, j| j.created.elapsed() < Duration::from_secs(86400) || lock(&j.state).busy());
    if jobs
        .values()
        .any(|j| j.agent == alias && lock(&j.state).busy())
    {
        return Err(ApiError::new(
            ErrorCode::Busy,
            summon_i18n::text!("r162d1ec671"),
        ));
    }
    jobs.retain(|_, j| j.agent != alias || j.principal != ctx.principal);
    if jobs.len() >= 8 {
        return Err(ApiError::new(
            ErrorCode::Busy,
            summon_i18n::text!("ra9e5011c78"),
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let job = Arc::new(Job {
        agent: alias,
        principal: ctx.principal.clone(),
        created: Instant::now(),
        state: Mutex::new(State::Preparing),
    });
    jobs.insert(id.clone(), job.clone());
    drop(jobs);
    let response = view(&id, &job);
    let work = job.clone();
    if let Err(error) = spawn(
        "agent-backup",
        Box::new(move || {
            let result = (|| {
                let _operation = env.operations.read().unwrap_or_else(|e| e.into_inner());
                let _manifest = crate::setup::bind::manifest_write_lock(&agent.manifest_path())?;
                agent_archive::backup(&env.home, &agent)
            })();
            *lock(&work.state) = match result {
                Ok(backup) => State::Ready {
                    backup,
                    downloaded: 0,
                },
                Err(e) => State::PreparationFailed {
                    message: format!("{e:#}"),
                },
            };
        }),
    ) {
        let message = error.to_string();
        *lock(&job.state) = State::PreparationFailed {
            message: message.clone(),
        };
        return Err(internal(message));
    }
    Ok(response)
}
pub(crate) fn status(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let JobParam { job: id } = params(p)?;
    Ok(view(&id, job(ctx, &id)?.as_ref()))
}
#[derive(Deserialize)]
struct Download {
    job: String,
    offset: u64,
}
pub(crate) fn download(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    let req: Download = params(p)?;
    let job = job(ctx, &req.job)?;
    let mut s = lock(&job.state);
    let State::Ready { backup, downloaded } = &mut *s else {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            summon_i18n::text!("r46fd089a5c"),
        ));
    };
    if req.offset > *downloaded || req.offset > backup.bytes {
        return Err(ApiError::new(
            ErrorCode::Invalid,
            summon_i18n::text!("r430c43bf08"),
        ));
    }
    let mut file = File::open(backup.path()).map_err(|e| internal(e.to_string()))?;
    file.seek(SeekFrom::Start(req.offset))
        .map_err(|e| internal(e.to_string()))?;
    let mut bytes = vec![0; (backup.bytes - req.offset).min(512 * 1024) as usize];
    file.read_exact(&mut bytes)
        .map_err(|e| internal(e.to_string()))?;
    let next = req.offset + bytes.len() as u64;
    let done = next == backup.bytes;
    *downloaded = (*downloaded).max(next);
    Ok(
        json!({"offset":req.offset,"next":next,"done":done,"base64":base64::engine::general_purpose::STANDARD.encode(bytes)}),
    )
}
#[derive(Deserialize)]
struct Confirm {
    job: String,
    saved: bool,
    agent: String,
}
pub(crate) fn confirm(ctx: &ApiCtx, p: Value) -> Result<Value, ApiError> {
    confirm_with_spawn(ctx, p, spawn)
}

fn confirm_with_spawn(
    ctx: &ApiCtx,
    p: Value,
    spawn: impl FnOnce(&str, Worker) -> std::io::Result<()>,
) -> Result<Value, ApiError> {
    let req: Confirm = params(p)?;
    let job = job(ctx, &req.job)?;
    if !req.saved || req.agent != job.agent {
        return Err(ApiError::new(
            ErrorCode::Invalid,
            summon_i18n::text!("r7dbac07952"),
        ));
    }
    let env = ctx.env_arc()?;
    let mut s = lock(&job.state);
    if matches!(*s, State::Done { .. }) {
        drop(s);
        return Ok(view(&req.job, &job));
    }
    let State::Ready { backup, downloaded } = &*s else {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            summon_i18n::text!("r0ddfdd8cdf"),
        ));
    };
    if *downloaded != backup.bytes {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            summon_i18n::text!("r301d3d7d02"),
        ));
    }
    let State::Ready { backup, .. } = std::mem::replace(&mut *s, State::Cleaning) else {
        unreachable!("Ready was checked under the same lock")
    };
    let response = view_state(&req.job, &job, &s);
    let work = job.clone();
    // spawn 失败会丢弃闭包；成功启动之后才转移备份，失败时仍能保留唯一的压缩包。
    // 状态锁覆盖交接，外界看到 Cleaning 时 worker 已接手，重复确认不能再取得 owner。
    let (sender, receiver) = std::sync::mpsc::channel::<Backup>();
    let launched = match spawn(
        "agent-cleanup",
        Box::new(move || {
            let Ok(backup) = receiver.recv() else { return };
            let result = clean(&env, &work, &backup);
            *lock(&work.state) = match result {
                Ok(message) => State::Done { message },
                Err(e) => State::CleanupFailed {
                    message: format!("{e:#}"),
                    backup,
                },
            };
        }),
    ) {
        Ok(()) => sender
            .send(backup)
            .map_err(|error| (internal(summon_i18n::text!("r94609edae7")), error.0)),
        Err(error) => Err((internal(error.to_string()), backup)),
    };
    if let Err((error, backup)) = launched {
        *s = State::CleanupFailed {
            message: error.message.clone(),
            backup,
        };
        return Err(error);
    }
    Ok(response)
}
fn clean(env: &ApiEnv, job: &Job, backup: &Backup) -> anyhow::Result<Option<String>> {
    let _operation = env.operations.write().unwrap_or_else(|e| e.into_inner());
    // 配置与清理共用锁，锁内复验权限；用户手改由内容快照复验兜底。
    let _global = crate::runtime::dashboard::writer::lock(
        &env.config_path(),
        summon_core::dashboard::RawKind::Config,
    )
    .map_err(|e| anyhow::anyhow!(summon_i18n::format!("r6466ec6f21")))?;
    let agent = allowed(env, &job.principal, &job.agent).map_err(|e| anyhow::anyhow!(e.message))?;
    let _manifest = crate::setup::bind::manifest_write_lock(&agent.manifest_path())?;
    let agent = allowed(env, &job.principal, &job.agent).map_err(|e| anyhow::anyhow!(e.message))?;
    anyhow::ensure!(
        !env.enabled(&agent.name),
        summon_i18n::format!("r8e2746fa8e")
    );
    anyhow::ensure!(
        agent_archive::plan(&env.home, &agent)? == backup.plan,
        summon_i18n::format!("r1ae413b82e")
    );
    // dispatcher 可能正在读取 availability / 定时器，先等它结束，再持存储锁做提交。
    let result = (|| {
        env.port.retire_agent(&agent).map_err(anyhow::Error::msg)?;
        let _events = crate::runtime::agent_retirement::EVENT_WRITERS
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let _comments = crate::runtime::agent_retirement::COMMENT_WRITERS
            .try_lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("r494cec4d4c")))?;
        let _outbox = crate::runtime::agent_retirement::OUTBOX_WRITERS
            .try_lock()
            .map_err(|_| anyhow::anyhow!(summon_i18n::format!("rd5d6bcfd02")))?;
        env.schedule_store()
            .remove_agent_with(&agent.name, &backup.plan.schedules, |commit| {
                let gate = crate::runtime::agent_availability::AgentAvailability::lock(
                    &env.run_dir().join("inbox"),
                    &agent.name,
                )?;
                anyhow::ensure!(!gate.enabled()?, summon_i18n::format!("r9653c013d4"));
                anyhow::ensure!(
                    agent_archive::plan_with_schedules(
                        &env.home,
                        &agent,
                        backup.plan.schedules.clone()
                    )? == backup.plan,
                    summon_i18n::format!("r91ecd984fe")
                );
                agent_archive::clean_files(&env.home, &backup.plan, commit)
            })
    })();
    match result {
        Ok(message) => {
            env.port.finish_agent_cleanup(&agent.name);
            Ok(message)
        }
        Err(e) => {
            env.port.restore_agent(&agent.name);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::runtime::dashboard::sessions::ChatKind;

    fn kit() -> Kit {
        let kit = Kit::new();
        kit.add_agent("sample", &agent_yaml("sample", "示例", "codex", ""));
        kit.add_agent("issuer", &agent_yaml("issuer", "入口", "codex", ""));
        std::fs::write(
            kit.agents_dir().join("sample/.env"),
            "DASH_TEST_SAMPLE_APP_ID=cli_0000000000000099\nDASH_TEST_SAMPLE_APP_SECRET=s\n",
        )
        .unwrap();
        super::super::agents::set_enabled(&kit.owner(), json!({"agent":"sample","enabled":false}))
            .unwrap();
        kit
    }
    fn wait(ctx: &ApiCtx, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let v = status(ctx, json!({"job":id})).unwrap();
            if !matches!(v["status"].as_str(), Some("preparing" | "cleaning")) {
                return v;
            }
            assert!(Instant::now() < deadline, "job did not finish: {v}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn ready(ctx: &ApiCtx) -> String {
        let v = prepare(ctx, json!({"agent":"sample"})).unwrap();
        let id = v["job"].as_str().unwrap().to_string();
        let v = wait(ctx, &id);
        assert_eq!(v["status"], "ready", "{v}");
        id
    }
    fn download_all(ctx: &ApiCtx, id: &str) -> Vec<u8> {
        let mut offset = 0;
        let mut bytes = Vec::new();
        loop {
            let v = download(ctx, json!({"job":id,"offset":offset})).unwrap();
            bytes.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(v["base64"].as_str().unwrap())
                    .unwrap(),
            );
            offset = v["next"].as_u64().unwrap();
            if v["done"] == true {
                break;
            }
        }
        bytes
    }

    fn no_backup(view: &Value, phase: &str) {
        assert_eq!(view["status"], phase, "{view}");
        for field in ["bytes", "sha256", "paths"] {
            assert!(view[field].is_null(), "{field}: {view}");
        }
        assert_eq!(view["downloaded"], false, "{view}");
    }

    fn refuse_spawn(_name: &str, _worker: Worker) -> std::io::Result<()> {
        Err(std::io::Error::other("worker unavailable"))
    }

    #[test]
    fn state_machine_api_preserves_views_and_gates_across_every_success_phase() {
        let kit = kit();
        let ctx = kit.owner();
        let env = kit.env();
        let preparing = env.operations.write().unwrap();
        let response = prepare(&ctx, json!({"agent":"sample"})).unwrap();
        let id = response["job"].as_str().unwrap();
        no_backup(&response, "preparing");
        no_backup(&status(&ctx, json!({"job":id})).unwrap(), "preparing");
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            prepare(&ctx, json!({"agent":"sample"})).unwrap_err().code,
            ErrorCode::Busy
        );
        drop(preparing);
        let ready = wait(&ctx, id);
        assert_eq!(ready["status"], "ready");
        assert!(ready["bytes"].as_u64().unwrap() > 0);
        assert!(ready["sha256"].is_string() && ready["paths"].is_array());
        assert_eq!(ready["filename"], "sample-backup.tar.gz");
        assert_eq!(ready["downloaded"], false);
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        download_all(&ctx, id);
        assert_eq!(status(&ctx, json!({"job":id})).unwrap()["downloaded"], true);
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":false}))
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"issuer","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );

        let cleaning = env.operations.read().unwrap();
        no_backup(
            &confirm(&ctx, json!({"job":id,"agent":"sample","saved":true})).unwrap(),
            "cleaning",
        );
        no_backup(&status(&ctx, json!({"job":id})).unwrap(), "cleaning");
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            prepare(&ctx, json!({"agent":"sample"})).unwrap_err().code,
            ErrorCode::Busy
        );
        drop(cleaning);
        let done = wait(&ctx, id);
        no_backup(&done, "done");
        assert!(!kit.agents_dir().join("sample").exists());
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true})).unwrap(),
            done
        );
    }

    #[test]
    fn state_machine_download_progress_requires_contiguous_bytes_and_allows_replay() {
        let kit = kit();
        let ctx = kit.owner();
        let mut seed = 0x51f8_0dba_u32;
        let bytes = (0..1_100_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect::<Vec<_>>();
        std::fs::write(kit.agents_dir().join("sample/payload"), bytes).unwrap();
        let id = ready(&ctx);
        let first = download(&ctx, json!({"job":id,"offset":0})).unwrap();
        let next = first["next"].as_u64().unwrap();
        assert_eq!(next, 512 * 1024);
        assert_eq!(first["done"], false);
        assert_eq!(
            status(&ctx, json!({"job":id})).unwrap()["downloaded"],
            false
        );
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":next+1}))
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
        assert_eq!(download(&ctx, json!({"job":id,"offset":0})).unwrap(), first);
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        download_all(&ctx, &id);
        let complete = status(&ctx, json!({"job":id})).unwrap();
        assert_eq!(complete["downloaded"], true);
        download(&ctx, json!({"job":id,"offset":0})).unwrap();
        assert_eq!(status(&ctx, json!({"job":id})).unwrap(), complete);
        let end = complete["bytes"].as_u64().unwrap();
        let eof = download(&ctx, json!({"job":id,"offset":end})).unwrap();
        assert_eq!(eof["done"], true);
        assert_eq!(eof["base64"], "");
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":end+1}))
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
    }

    #[test]
    fn state_machine_failed_cleanup_keeps_backup_metadata_without_enabling_retry() {
        let kit = kit();
        let ctx = kit.feishu(issuer("issuer", P2P, ChatKind::P2p));
        let id = ready(&ctx);
        download_all(&ctx, &id);
        let ready = status(&ctx, json!({"job":id})).unwrap();
        std::fs::write(kit.agents_dir().join("sample/new"), b"changed").unwrap();
        no_backup(
            &confirm(&ctx, json!({"job":id,"agent":"sample","saved":true})).unwrap(),
            "cleaning",
        );
        let failed = wait(&ctx, &id);
        assert_eq!(failed["status"], "failed");
        assert!(failed["message"].as_str().unwrap().contains("数据已变化"));
        for field in ["bytes", "sha256", "paths", "downloaded"] {
            assert_eq!(failed[field], ready[field], "{field}");
        }
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert!(kit.agents_dir().join("sample/new").is_file());
    }

    #[test]
    fn state_machine_backup_access_remains_bound_to_creator_and_current_permissions() {
        let kit = kit();
        let ctx = kit.feishu(issuer("issuer", P2P, ChatKind::P2p));
        let other = kit.feishu(issuer("issuer", GROUP, ChatKind::Group));
        let id = ready(&ctx);
        assert_eq!(
            status(&other, json!({"job":id})).unwrap_err().code,
            ErrorCode::Forbidden
        );
        assert_eq!(
            download(&other, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        assert_eq!(
            confirm(&other, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        let path = kit.agents_dir().join("sample/agent.yaml");
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, original.replace(ME_UNION, PEER_UNION)).unwrap();
        assert_eq!(
            status(&ctx, json!({"job":id})).unwrap_err().code,
            ErrorCode::Forbidden
        );
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        std::fs::write(&path, original).unwrap();
        assert_eq!(status(&ctx, json!({"job":id})).unwrap()["status"], "ready");
    }

    #[test]
    fn state_machine_prepare_spawn_failure_is_terminal_and_releases_busy_slot() {
        let kit = kit();
        let ctx = kit.owner();
        assert_eq!(
            prepare_with_spawn(&ctx, json!({"agent":"sample"}), refuse_spawn)
                .unwrap_err()
                .code,
            ErrorCode::Internal
        );
        let id = lock(&kit.env().cleanup).keys().next().unwrap().clone();
        let failed = status(&ctx, json!({"job":id})).unwrap();
        no_backup(&failed, "failed");
        assert!(failed["message"]
            .as_str()
            .unwrap()
            .contains("worker unavailable"));
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        let next = ready(&ctx);
        assert_ne!(next, id);
        assert!(kit.agents_dir().join("sample").is_dir());
    }

    #[test]
    fn state_machine_backup_failure_has_no_downloadable_owner() {
        let kit = kit();
        let ctx = kit.owner();
        let path = kit.home().join("run/document-comments/unknown.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"invalid JSON").unwrap();
        let response = prepare(&ctx, json!({"agent":"sample"})).unwrap();
        let id = response["job"].as_str().unwrap();
        let failed = wait(&ctx, id);
        no_backup(&failed, "failed");
        assert!(failed["message"]
            .as_str()
            .unwrap()
            .contains("无法判定记录归属"));
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        std::fs::remove_file(path).unwrap();
        assert_ne!(ready(&ctx), id);
        assert!(kit.agents_dir().join("sample").is_dir());
    }

    #[test]
    fn state_machine_lost_worker_preserves_the_unaccepted_backup() {
        let kit = kit();
        let ctx = kit.owner();
        let id = ready(&ctx);
        download_all(&ctx, &id);
        let before = status(&ctx, json!({"job":id})).unwrap();
        assert_eq!(
            confirm_with_spawn(
                &ctx,
                json!({"job":id,"agent":"sample","saved":true}),
                |_, _| Ok(())
            )
            .unwrap_err()
            .code,
            ErrorCode::Internal
        );
        let failed = status(&ctx, json!({"job":id})).unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["message"], "清理任务未接收备份");
        for field in ["bytes", "sha256", "paths", "downloaded"] {
            assert_eq!(failed[field], before[field], "{field}");
        }
        assert!(kit.agents_dir().join("sample").is_dir());
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
    }

    #[test]
    fn state_machine_cleanup_spawn_failure_preserves_backup_and_cannot_clean() {
        let kit = kit();
        let ctx = kit.owner();
        let id = ready(&ctx);
        download_all(&ctx, &id);
        let before = status(&ctx, json!({"job":id})).unwrap();
        assert_eq!(
            confirm_with_spawn(
                &ctx,
                json!({"job":id,"agent":"sample","saved":true}),
                refuse_spawn
            )
            .unwrap_err()
            .code,
            ErrorCode::Internal
        );
        let failed = status(&ctx, json!({"job":id})).unwrap();
        assert_eq!(failed["status"], "failed");
        assert!(failed["message"]
            .as_str()
            .unwrap()
            .contains("worker unavailable"));
        for field in ["bytes", "sha256", "paths", "downloaded"] {
            assert_eq!(failed[field], before[field], "{field}");
        }
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert!(kit.agents_dir().join("sample").is_dir());
        assert_ne!(ready(&ctx), id);
    }

    #[test]
    fn cleanup_requires_complete_download_and_saved_confirmation_then_is_idempotent() {
        let kit = kit();
        let ctx = kit.owner();
        let id = ready(&ctx);
        assert!(confirm(&ctx, json!({"job":id,"agent":"sample","saved":true})).is_err());
        assert!(download(&ctx, json!({"job":id,"offset":1})).is_err());
        let bytes = download_all(&ctx, &id);
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
        assert!(tar
            .entries()
            .unwrap()
            .any(|e| e.unwrap().path().unwrap() == PathBuf::from("summon/agents/sample/.env")));
        assert!(confirm(&ctx, json!({"job":id,"agent":"sample","saved":false})).is_err());
        assert!(confirm(&ctx, json!({"job":id,"agent":"issuer","saved":true})).is_err());
        confirm(&ctx, json!({"job":id,"agent":"sample","saved":true})).unwrap();
        let v = wait(&ctx, &id);
        assert_eq!(v["status"], "done", "{v}");
        assert!(!kit.agents_dir().join("sample").exists());
        assert!(kit.agents_dir().join("issuer").exists());
        kit.add_agent("sample", &agent_yaml("sample", "新示例", "codex", ""));
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true})).unwrap()["status"],
            "done"
        );
        assert!(kit.agents_dir().join("sample").exists());
    }

    #[test]
    fn cleanup_rechecks_disabled_state_data_and_current_permissions() {
        let kit = kit();
        let ctx = kit.feishu(issuer("issuer", P2P, ChatKind::P2p));
        let id = ready(&ctx);
        download_all(&ctx, &id);
        std::fs::write(kit.agents_dir().join("sample/new"), b"new").unwrap();
        confirm(&ctx, json!({"job":id,"agent":"sample","saved":true})).unwrap();
        assert_eq!(wait(&ctx, &id)["status"], "failed");
        assert!(kit.agents_dir().join("sample/new").exists());
        let id = ready(&ctx);
        download_all(&ctx, &id);
        super::super::agents::set_enabled(&kit.owner(), json!({"agent":"sample","enabled":true}))
            .unwrap();
        confirm(&ctx, json!({"job":id,"agent":"sample","saved":true})).unwrap();
        assert_eq!(wait(&ctx, &id)["status"], "failed");
        super::super::agents::set_enabled(&kit.owner(), json!({"agent":"sample","enabled":false}))
            .unwrap();
        let id = ready(&ctx);
        download_all(&ctx, &id);
        let path = kit.agents_dir().join("sample/agent.yaml");
        std::fs::write(
            &path,
            std::fs::read_to_string(&path)
                .unwrap()
                .replace(ME_UNION, PEER_UNION),
        )
        .unwrap();
        assert_eq!(
            download(&ctx, json!({"job":id,"offset":0}))
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        assert_eq!(
            confirm(&ctx, json!({"job":id,"agent":"sample","saved":true}))
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        assert!(path.is_file());
    }

    #[test]
    fn cleanup_rejects_live_agents_unlisted_private_chat_and_other_job_owners() {
        let kit = kit();
        let owner = kit.owner();
        assert_eq!(
            prepare(&owner, json!({"agent":"issuer"})).unwrap_err().code,
            ErrorCode::Conflict
        );
        let ctx = kit.feishu(issuer("sample", P2P, ChatKind::P2p));
        assert_eq!(
            prepare(&ctx, json!({"agent":"sample"})).unwrap_err().code,
            ErrorCode::Forbidden
        );
        let id = ready(&owner);
        assert_eq!(
            status(&ctx, json!({"job":id})).unwrap_err().code,
            ErrorCode::Forbidden
        );
        let agent = kit.agents_dir().join("sample/agent.yaml");
        std::fs::write(&agent, "name: sample\nfeishu: {}\n").unwrap();
        let ctx = kit.feishu(issuer("issuer", P2P, ChatKind::P2p));
        assert_eq!(
            prepare(&ctx, json!({"agent":"sample"})).unwrap_err().code,
            ErrorCode::Forbidden
        );
    }
    use std::path::PathBuf;
}
