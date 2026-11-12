//! 工程表情(working/done)的**文件化状态队列**(slice 3 IO;纯决策 `resolve_done` 在
//! `summon_core::reactions`)。仿 `inbox_wal`:每条在飞 working 一个 `working/<message_id>.json`,记
//! message_id + `delivered_at`(消息真送达 claude 的时刻)+ OnIt 表情 id + chat 定位;done 即删该文件。
//!
//! 为什么文件化:① 会话镜像 pub-sub 后「哪条飞书消息该 done」靠 `resolve_done(list, completed_at)` 而非
//! `run_turn` 返回;② 支持同时多条在飞;③ **跨 daemon 重启可恢复** —— 重启后 OnIt 不再永远卡着,启动
//! 扫描据此把孤儿 working 收尾。key = message_id → 幂等覆盖。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::runtime::fsio::atomic_write;

/// 一条在飞工程表情记录(落 `working/<id>.json`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReactionRecord {
    pub(crate) message_id: String,
    /// 表情实际贴在哪条飞书消息上。`None` = 就是 `message_id` 本身。
    ///
    /// 恢复重放轮的 `message_id` 是合成的 `recovery-run-…`(本地键,`summon react` 按本轮凭证
    /// 查的也是它),但表情只能贴在**原消息**上 —— 两者必须分开存,否则撤 OnIt / 贴语义表情时
    /// 拿合成 id 去调,飞书一律 400,消息上会留一个撤不掉的 👀。
    /// `#[serde(default)]`:旧记录无此字段。
    #[serde(default)]
    pub(crate) anchor_message_id: Option<String>,
    /// 消息真正注入 claude 的时刻(毫秒)——done 归因据它(非飞书收到那刻)。
    pub(crate) delivered_at_ms: i64,
    /// 贴 OnIt 时拿到的 reaction id,done/恢复时据它 unreact(故跨重启也能撤掉 OnIt)。
    pub(crate) onit_reaction_id: String,
    pub(crate) chat_id: String,
    pub(crate) chat_type: String,
    /// agent 经 `summon react` 主动贴的**语义表情** id(可改可撤、单值):重贴时据它撤旧、`--clear` 撤掉。
    /// 与工程 OnIt/DONE 两条独立轨。`#[serde(default)]`:旧记录无此字段。
    #[serde(default)]
    pub(crate) agent_reaction_id: Option<String>,
}

pub(crate) struct ReactionStore {
    root: PathBuf,
}

impl ReactionRecord {
    /// 贴 / 撤表情时要用的那个 id。
    pub(crate) fn anchor(&self) -> &str {
        self.anchor_message_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or(&self.message_id)
    }
}

impl ReactionStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// per-agent 生产路径:`agents/<alias>/runtime/reactions`。**必须按 agent 隔离**:多个 agent 同处
    /// 一个飞书群(同 `chat_id`)时,若共用全局 run 目录,`find_by_chat` 只按 chat_id 查会**串号**——
    /// 后贴表情的 agent B 命中 agent A 写入的 `message_id`,把表情贴到 A 处理的消息上(真机 bug:
    /// blue-wing 进群 react DONE 贴到了 arcadia 的消息)。per-agent 目录天然隔离,与「身份隔离
    /// fail-closed」原则一致(记录本就无 agent 字段,靠路径隔离而非字段过滤)。
    pub(crate) fn for_agent(agent_dir: &Path) -> Self {
        Self::new(agent_dir.join("runtime").join("reactions"))
    }

    fn working_dir(&self) -> PathBuf {
        self.root.join("working")
    }

    fn path(&self, message_id: &str) -> PathBuf {
        self.working_dir()
            .join(format!("{}.json", sanitize_id(message_id)))
    }

    /// 入 working(消息真送达 claude 时)。原子落盘;空 id 跳过。best-effort,失败 log-ignore 由调用方。
    pub(crate) fn enqueue(&self, rec: &ReactionRecord) {
        if rec.message_id.trim().is_empty() {
            return;
        }
        if let Ok(json) = serde_json::to_string(rec) {
            if let Err(e) = atomic_write(&self.path(&rec.message_id), json.as_bytes()) {
                crate::observe::log::log(&summon_i18n::format!("r4ce4913c1b"));
            }
        }
    }

    /// done / 恢复后:从队列删一条。best-effort(不存在 = no-op)。
    pub(crate) fn remove(&self, message_id: &str) {
        let _ = std::fs::remove_file(self.path(message_id));
    }

    /// 列出所有在飞 working(损坏文件跳过;目录不存在 = 空)。启动恢复扫描用;会话镜像 pub-sub 路
    /// 的 done 归因把它 map 成 `summon_core::reactions::PendingReaction` 喂 `resolve_done`(slice 3 后续接)。
    pub(crate) fn list(&self) -> Vec<ReactionRecord> {
        read_all(&self.working_dir())
    }

    /// 找某 chat「当前正回应的那条」working 记录(owner 决策:agent 语义表情贴在它上面)。per-chat
    /// 串行通常只一条;并发取**最新送达**(delivered_at 最大)那条。无 → None。
    /// 按**精确 message_id** 取记录。**这是唯一的查法** —— 曾经的 `find_by_chat`
    /// (按 chat 取最新送达的一条)已删除:同一 chat 连发多条或轮次交错时它会贴到别的消息上,
    /// 而本轮凭证里既然带着确切 id,就没有理由再猜。
    ///
    /// `summon react` 用这条而不是 `find_by_chat`:后者取的是「这个 chat 里最新送达的一条」,
    /// 那在同一 chat 连发多条、或轮次交错时会贴到**别的消息**上。本轮凭证里带着确切的
    /// message_id,没有理由再去猜(2026-08-14 review 指出)。
    pub(crate) fn find_by_message(&self, message_id: &str) -> Option<ReactionRecord> {
        self.list().into_iter().find(|r| r.message_id == message_id)
    }
}

/// 文件名安全化:非字母数字 `_-` 一律换 `_`(message_id 形如 `om_xxx`,本就安全,防御性)。
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn read_all(dir: &Path) -> Vec<ReactionRecord> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "json") {
                if let Ok(s) = std::fs::read_to_string(e.path()) {
                    if let Ok(r) = serde_json::from_str::<ReactionRecord>(&s) {
                        out.push(r);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::reactions::{resolve_done, PendingReaction};

    fn rec(id: &str, t: i64) -> ReactionRecord {
        ReactionRecord {
            anchor_message_id: None,
            message_id: id.into(),
            delivered_at_ms: t,
            onit_reaction_id: format!("rid_{id}"),
            chat_id: "oc_1".into(),
            chat_type: "p2p".into(),
            agent_reaction_id: None,
        }
    }

    /// 把 store.list() map 成纯决策要的视图(会话镜像 pub-sub 路 done 归因即这么用)。
    fn pending(store: &ReactionStore) -> Vec<PendingReaction> {
        store
            .list()
            .into_iter()
            .map(|r| PendingReaction {
                message_id: r.message_id,
                delivered_at_ms: r.delivered_at_ms,
            })
            .collect()
    }

    #[test]
    fn enqueue_list_resolve_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReactionStore::new(dir.path().to_path_buf());
        store.enqueue(&rec("om_a", 1000));
        store.enqueue(&rec("om_b", 2000));
        // list 落盘可读回(含 onit id + chat 定位,done/恢复时要用)。
        let a = store
            .list()
            .into_iter()
            .find(|r| r.message_id == "om_a")
            .unwrap();
        assert_eq!(a.onit_reaction_id, "rid_om_a");
        // pending → resolve_done:完成 2500 取最早已送达 = om_a。
        assert_eq!(resolve_done(&pending(&store), 2500), Some("om_a"));
        // done 清理后,下次 resolve 取 om_b。
        store.remove("om_a");
        assert_eq!(resolve_done(&pending(&store), 2600), Some("om_b"));
        store.remove("om_b");
        assert!(store.list().is_empty());
    }

    #[test]
    fn empty_id_and_missing_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReactionStore::new(dir.path().to_path_buf());
        store.enqueue(&rec("", 1)); // 空 id 不入
        store.remove("never"); // 不存在 no-op
        assert!(store.list().is_empty());
    }

    #[test]
    /// 按**精确 message_id** 取,不按 chat 猜最新。
    ///
    /// 这条替换了原来的 `find_by_chat_picks_latest_delivered_and_isolates_chats`:那个查法
    /// 取「本 chat 内 delivered_at 最大的一条」,在同一 chat 连发多条或轮次交错时会把表情贴到
    /// **别的消息**上。本轮凭证里带着确切 message_id,不需要猜(2026-08-14 review)。
    fn find_by_message_targets_the_exact_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = ReactionStore::new(dir.path().to_path_buf());
        store.enqueue(&rec("om_a", 1000));
        store.enqueue(&rec("om_b", 3000));
        let mut other = rec("om_c", 9000);
        other.chat_id = "oc_2".into();
        store.enqueue(&other);
        // 取的就是那一条 —— 即便同 chat 里有更晚送达的。
        assert_eq!(store.find_by_message("om_a").unwrap().message_id, "om_a");
        assert_eq!(store.find_by_message("om_b").unwrap().message_id, "om_b");
        // 别的 chat 的记录照样按 id 命中(id 全局唯一,不需要再按 chat 过滤)。
        assert_eq!(store.find_by_message("om_c").unwrap().chat_id, "oc_2");
        // 没有这条 → None,调用方据此静默跳过。
        assert!(store.find_by_message("om_missing").is_none());
    }

    #[test]
    fn for_agent_isolates_same_chat_across_agents() {
        // 根因回归(真机 bug):arcadia 与 blue-wing 在同一飞书群(同 chat_id)。per-agent store 各自
        // 只见自己的 working 记录——否则 find_by_chat 只按 chat_id 查会串号,B 的 react 贴到 A 写入的
        // message_id 上。用两个 agent_dir 走 for_agent,验证隔离。
        let da = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let sa = ReactionStore::for_agent(da.path());
        let sb = ReactionStore::for_agent(db.path());
        let mut ra = rec("om_arcadia", 1000);
        ra.chat_id = "oc_group".into();
        sa.enqueue(&ra);
        // agent B(不同 agent_dir)查不到 A 的记录 —— per-agent 隔离成立,不再串号。
        // 按 message_id 查更能说明问题:即便 id 全局唯一,B 的 store 里也不该有 A 的记录。
        assert!(sb.find_by_message("om_arcadia").is_none());
        assert_eq!(
            sa.find_by_message("om_arcadia").unwrap().chat_id,
            "oc_group"
        );
    }
}
