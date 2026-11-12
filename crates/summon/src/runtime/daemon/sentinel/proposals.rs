//! 三方建群 proposal 存根——确认卡与执行之间的持久握手。
//!
//! 确认卡 callback value 只带 `{action, proposal}` 两个 opaque 字段;对方 open_id/原单聊/
//! 触发正文等真实参数全部在盘上存根里,handler 取回后做 fencing 复验(状态机
//! pending→done/dismissed + 30min TTL + card message_id 精确匹配),点两次/重放都幂等。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 确认卡有效期。过期点击只把卡置灰,绝不执行——提案描述的是「当时」的意图,过期后
/// 上下文可能已完全不同。
pub(crate) const PROPOSAL_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub(crate) enum ProposalState {
    Pending,
    Done { chat_id: String },
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Proposal {
    pub(crate) id: String,
    pub(crate) agent_name: String,
    /// 原单聊 chat_id(chat_id 是租户级标识,lark-cli 与 agent app 通用)。
    pub(crate) p2p_chat_id: String,
    /// 对方 open_id(**lark-cli 命名空间**,只用于 owner 身份拉人)。
    pub(crate) counterpart_open_id: String,
    pub(crate) counterpart_name: String,
    /// 触发正文(剥掉动作词的部分)。**只用来判断 owner 附问题了没有**,当 prompt 用
    /// [`Proposal::ask_verbatim`]。
    pub(crate) body: String,
    /// 触发的完整原话(含动作词)。2026-08-12 新增:动作词按前缀剥会改语义
    /// (「来帮我看下这个 CR」→「我看下这个 CR」)。
    ///
    /// `serde(default)` 是为**盘上的旧提案**:它们没有这个字段,反序列化得到空串,
    /// `ask_verbatim` 会回退到 `body`,保持旧行为而不是崩掉或发一句空 prompt。
    #[serde(default)]
    pub(crate) verbatim: String,
    pub(crate) trigger_message_id: String,
    /// ChatTask 的 who 三元组(who_id 为 **agent 命名空间** 的 owner open_id)。
    pub(crate) who: String,
    pub(crate) who_id: String,
    pub(crate) who_union_id: String,
    pub(crate) created_ms: i64,
    pub(crate) state: ProposalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) card_message_id: Option<String>,
}

impl Proposal {
    /// 交给 agent / 卡片的原话。旧提案没有 `verbatim`,回退 `body`(旧行为)。
    pub(crate) fn ask_verbatim(&self) -> &str {
        if self.verbatim.is_empty() {
            &self.body
        } else {
            &self.verbatim
        }
    }
}

pub(crate) struct ProposalStore {
    path: PathBuf,
}

impl ProposalStore {
    pub(crate) fn at_run_dir() -> Self {
        Self {
            path: crate::runtime::paths::run_dir().join("sentinel-proposals.json"),
        }
    }

    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// 文件不存在 → 空表;损坏 → Err(fail-closed:读不出真相就不执行任何提案)。
    pub(crate) fn load(&self) -> Result<Vec<Proposal>> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => serde_json::from_str(&s)
                .with_context(|| summon_i18n::format!("r9e5398748c", self.path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e).context(summon_i18n::text!("rda05c8be0f")),
        }
    }

    pub(crate) fn save(&self, items: &[Proposal]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(items)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub(crate) fn insert(&self, p: Proposal) -> Result<()> {
        let mut items = self.load()?;
        items.push(p);
        self.save(&items)
    }

    /// 就地改一条并落盘;返回改后的克隆(没找到 → None)。
    pub(crate) fn update(
        &self,
        id: &str,
        f: impl FnOnce(&mut Proposal),
    ) -> Result<Option<Proposal>> {
        let mut items = self.load()?;
        let Some(p) = items.iter_mut().find(|p| p.id == id) else {
            return Ok(None);
        };
        f(p);
        let out = p.clone();
        self.save(&items)?;
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> Proposal {
        Proposal {
            id: id.to_string(),
            agent_name: "arcadia".into(),
            p2p_chat_id: "oc_p2p".into(),
            counterpart_open_id: "ou_other".into(),
            counterpart_name: "张三".into(),
            body: "回答这些消息".into(),
            verbatim: "进来回答这些消息".into(),
            trigger_message_id: "om_t".into(),
            who: "张三".into(),
            who_id: "ou_owner".into(),
            who_union_id: String::new(),
            created_ms: 1000,
            state: ProposalState::Pending,
            card_message_id: Some("om_card".into()),
        }
    }

    #[test]
    fn 存根_落盘往返与更新() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProposalStore::at(tmp.path().join("p.json"));
        assert!(store.load().unwrap().is_empty());
        store.insert(sample("sp-1")).unwrap();
        store.insert(sample("sp-2")).unwrap();
        let got = store.load().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].state, ProposalState::Pending);

        let updated = store
            .update("sp-1", |p| {
                p.state = ProposalState::Done {
                    chat_id: "oc_new".into(),
                }
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.state,
            ProposalState::Done {
                chat_id: "oc_new".into()
            }
        );
        assert!(store.update("sp-404", |_| {}).unwrap().is_none());
    }

    #[test]
    fn 存根_损坏文件报错不静默() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(ProposalStore::at(path).load().is_err());
    }
}
