//! 飞书凭据身份漂移判定(纯逻辑)。
//!
//! 背景:daemon 启动时冻结每个 agent 的 resolved 飞书身份(只留 SHA-256,不留凭据),之后盘上
//! `.env` / `agent.yaml` 一旦漂移就对该 agent 的外部入站 **fail-closed**(见 dispatcher)。语义本身
//! 是对的,坏在**只有 daemon.log 里有一行**——用户的排查路径是「发消息没反应 → `summon doctor`」,
//! 而 doctor 只验「盘上凭据在不在」,于是报全绿:**agent 聋了,唯一的诊断工具说一切正常**。
//!
//! 这里只做判定,不碰 IO:指纹怎么算(daemon 侧)、怎么问(control.sock)、怎么渲染(doctor)
//! 都在 `summon` 层。抽成纯函数是为了能脱离 daemon 单测「一致 / 已漂移 / 无法确认」三态。
//!
//! **不变量:指纹是摘要,不是凭据,也永不进用户可见输出。** 判定结果只说一致/漂移。

use serde::{Deserialize, Serialize};

/// daemon epoch 为某 agent 冻结的飞书身份指纹(SHA-256 hex)。
/// 随 `RuntimeStatus` 经 control.sock 回给本机 CLI —— 只是摘要,反推不出凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCredentialIdentity {
    /// agent 名(与 `RuntimeStatus.busy[].agent`、registry 的 `name` 同一维度)。
    pub agent: String,
    /// 该 agent 在 daemon 冻结时刻的身份指纹(hex)。
    pub fingerprint: String,
}

/// 「无法确认」的具体原因。分开是因为三种情况的下一步动作完全不同:
/// daemon 没起就先起 daemon,daemon 没上报多半是版本没对齐,盘上算不出才是配置坏了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityUnknown {
    /// daemon 不在跑——没有内存态可比,降级而不是报错。
    DaemonDown,
    /// daemon 在跑但没上报该 agent(dispatcher 尚未起 / daemon 是不带该字段的旧版本)。
    NotReported,
    /// 盘上身份算不出(凭据缺失、agent.yaml 读不出)——另有 app_id/secret 行会报。
    DiskUnreadable,
}

/// daemon 内存中的凭据身份 vs 盘上凭据身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialIdentity {
    /// 一致:daemon 用的就是盘上这份。
    Match,
    /// 已漂移:daemon 仍持旧凭据,该 agent 的外部入站已 fail-closed,重启前收不到任何飞书消息。
    Drifted,
    /// 无法确认(不等于正常,也不等于故障)。
    Unknown(IdentityUnknown),
}

impl CredentialIdentity {
    pub fn is_drifted(self) -> bool {
        matches!(self, CredentialIdentity::Drifted)
    }
}

/// 判定某 agent 的凭据身份状态。
///
/// 三态取舍:
/// - daemon 不在 → `Unknown(DaemonDown)`。**不能报绿**:没有内存态可比,「一致」是编出来的。
/// - 指纹任一侧缺失(含空串)→ `Unknown`。空串相等会把「两边都算不出」误判成一致,
///   这是本模块唯一的 fail-open 风险,故在入口就把空串归一成缺失。
/// - 两侧都在 → 逐字节比,不等即 `Drifted`。
pub fn classify_credential_identity(
    daemon_running: bool,
    daemon_fingerprint: Option<&str>,
    disk_fingerprint: Option<&str>,
) -> CredentialIdentity {
    if !daemon_running {
        return CredentialIdentity::Unknown(IdentityUnknown::DaemonDown);
    }
    let daemon = daemon_fingerprint.filter(|s| !s.is_empty());
    let disk = disk_fingerprint.filter(|s| !s.is_empty());
    match (daemon, disk) {
        (None, _) => CredentialIdentity::Unknown(IdentityUnknown::NotReported),
        (Some(_), None) => CredentialIdentity::Unknown(IdentityUnknown::DiskUnreadable),
        (Some(a), Some(b)) if a == b => CredentialIdentity::Match,
        _ => CredentialIdentity::Drifted,
    }
}

/// 从 daemon 上报的清单里取某 agent 的指纹。
pub fn fingerprint_of<'a>(
    identities: &'a [AgentCredentialIdentity],
    agent: &str,
) -> Option<&'a str> {
    identities
        .iter()
        .find(|entry| entry.agent == agent)
        .map(|entry| entry.fingerprint.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP_A: &str = "aaaa1111bbbb2222cccc3333dddd4444eeee5555ffff6666aaaa7777bbbb8888";
    const FP_B: &str = "9999000011112222333344445555666677778888999900001111222233334444";

    #[test]
    fn same_fingerprint_is_match() {
        assert_eq!(
            classify_credential_identity(true, Some(FP_A), Some(FP_A)),
            CredentialIdentity::Match
        );
    }

    #[test]
    fn different_fingerprint_is_drifted() {
        let state = classify_credential_identity(true, Some(FP_A), Some(FP_B));
        assert_eq!(state, CredentialIdentity::Drifted);
        assert!(state.is_drifted());
    }

    #[test]
    fn daemon_down_degrades_to_unknown_not_match() {
        // 真机踩到的那次:doctor 报全绿而 agent 是聋的。daemon 不在时更没有内存态可比,
        // 绝不能因为「两边指纹都拿不到」而落进相等分支。
        assert_eq!(
            classify_credential_identity(false, Some(FP_A), Some(FP_A)),
            CredentialIdentity::Unknown(IdentityUnknown::DaemonDown)
        );
        assert_eq!(
            classify_credential_identity(false, None, None),
            CredentialIdentity::Unknown(IdentityUnknown::DaemonDown)
        );
    }

    #[test]
    fn missing_side_never_counts_as_match() {
        assert_eq!(
            classify_credential_identity(true, None, Some(FP_A)),
            CredentialIdentity::Unknown(IdentityUnknown::NotReported)
        );
        assert_eq!(
            classify_credential_identity(true, Some(FP_A), None),
            CredentialIdentity::Unknown(IdentityUnknown::DiskUnreadable)
        );
    }

    #[test]
    fn empty_fingerprints_are_treated_as_missing() {
        // 两侧都是空串会逐字节相等 → 假绿。空串必须先归一成缺失。
        assert_eq!(
            classify_credential_identity(true, Some(""), Some("")),
            CredentialIdentity::Unknown(IdentityUnknown::NotReported)
        );
        assert_eq!(
            classify_credential_identity(true, Some(FP_A), Some("")),
            CredentialIdentity::Unknown(IdentityUnknown::DiskUnreadable)
        );
    }

    #[test]
    fn fingerprint_lookup_is_per_agent() {
        let identities = vec![
            AgentCredentialIdentity {
                agent: "arcadia".into(),
                fingerprint: FP_A.into(),
            },
            AgentCredentialIdentity {
                agent: "starlight".into(),
                fingerprint: FP_B.into(),
            },
        ];
        assert_eq!(fingerprint_of(&identities, "arcadia"), Some(FP_A));
        assert_eq!(fingerprint_of(&identities, "starlight"), Some(FP_B));
        // fail-closed 是 per-agent 的:清单里没有的 agent 不能借别人的结论。
        assert_eq!(fingerprint_of(&identities, "nobody"), None);
    }
}
