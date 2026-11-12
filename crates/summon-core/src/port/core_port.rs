//! Core Port:入口无关的命令面(进)+ 事件面(出)。飞书、agent CLI 和人用 CLI 都翻译成
//! Command 投进来，订阅 CoreEvent 出去。
//!
//! 这里只放纯类型与 trait;Core 的实现(会话编排 / 闸门 / operate-gate)在
//! `summon` 层(碰 IO / tmux / 进程)。

use crate::port::ids::{ChatId, GateId, MsgRef};
use serde::{Deserialize, Serialize};

/// 谁发起的命令 —— operate-gate 的鉴权依据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Actor {
    /// 飞书用户(open_id)。
    Feishu(String),
    /// 本机 owner（终端 CLI）——本地进程可信。
    Owner,
    /// 某 chat turn 内的 agent。CLI 从短生命周期 CurrentTurnStore fail-closed 读出来源，
    /// Core 再校验 alias/source 与本轮活跃凭证的一致性。
    /// 装箱:它比另两个 variant 大一个量级,内联会让每个 `Actor` 都按它的尺寸占位;
    /// `Box` 对 serde 透明,control.sock 上的 JSON 与内联字段时逐字节相同。
    ChatAgent(Box<ChatAgentIdentity>),
}

/// chat 轮内 agent 的自证身份。它是 `summon ask` 在 control.sock 上出示的全部凭据 ——
/// 缺任何一项 Core 都认不出「是哪个 agent 的哪个会话的哪一轮在问」,只能 fail-closed。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatAgentIdentity {
    /// 动态 prompt 给出的本轮随机凭证；Core 只接受本代 daemon 当前仍活跃的一条。
    pub turn_id: String,
    pub alias: String,
    pub requester_open_id: String,
    pub source_ref: MsgRef,
    pub chat_type: String,
    pub conversation_tag: String,
}

/// 拆除某 chat 会话的力度。Clean = 归档 `.session`(下次可 resume 续聊);
/// Reset = 归档 + 清 `.session`(下次冷启=真重置)。与面板 clean/reset 同义,
/// 但走 control.sock 让 `agents kill` 对无头(cursor/codex)会话也能拆。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeardownMode {
    Clean,
    Reset,
}

/// 一切「写意图」的统一表达。新增前端动作时挂这里,不在多处散落 match。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Intent {
    /// 人点卡片解闸门(提交答案 / 全部跳过)。
    QuestionResolve(GateId, String),
    /// 人取消闸门(agent 收到后应停止当前动作、回到待命)。
    CancelGate(GateId),
    /// 飞书入站普通消息(走 chat 会话)。
    ChatMessage { chat: ChatId, text: String },
    /// 拆某 agent 的某 chat 会话(`agents kill` 经 control.sock 走的路径)。无头(cursor/codex)
    /// 会话无 tmux、实体只活在 daemon 内,故 CLI 必须经此让 daemon 打断在跑子进程 + 拆 dispatcher
    /// handle + 归档 `.session`。claude(tmux)仍走 CLI 本地 `Tmux::kill`,不经此。
    TeardownChat {
        alias: String,
        chat_id: ChatId,
        mode: TeardownMode,
    },
    /// Menu Bar 的 Exit / Archive:结束某 agent 的某 chat 会话。`archive=false` 是退出(保留续聊
    /// 入口),`true` 是归档(下条消息新建会话,`/resume` 可恢复)。投到与管家卡「退出 / 📦 归档」
    /// 同一条 dispatcher 通道,清 waiting、打断在跑的轮、退出回执都与那两个按钮一致。
    /// Owner-only(本机 CLI),不过 operate-gate。
    EndChatSession {
        alias: String,
        chat_id: ChatId,
        archive: bool,
    },
    /// 开机自启(启动项)开/关。`enabled=false` = 关闭;`keep_alive` 仅 enable 时有意义(连正常退出
    /// 也拉起,opt-in)。Owner-only(本机可信,改的是本机 launchd LaunchAgent),不过 operate-gate。
    /// CLI 本地直接做；其它本机控制入口也必须经此 Command 路由到 daemon 内 Core 执行。
    SetAutostart { enabled: bool, keep_alive: bool },
    /// 本机 owner 改某 agent 的 model：写 agent.yaml，下条消息起经 `reread` 生效。
    SetModel { alias: String, model: String },
    /// 本机 owner 改某 agent 的 effort：写 agent.yaml，下条消息起生效。写前校验。
    SetEffort { alias: String, effort: String },
    /// 扫 agents 目录,把「盘上可路由、daemon 还不认识」的 agent 接上线(路由 + gate 快照 +
    /// dispatcher 线程 + outbox/scheduler 表),并让 sidecar 重启以建立新 app 的飞书长连接。
    /// Owner-only(本机 onboard 建完 agent 后调)。**只新增,不移除/改名** —— 后者要停 dispatcher
    /// 线程并处理它手上在跑的轮,是另一个量级的问题,仍走 `daemon restart`。
    /// why 不直接 restart:`daemon stop` 会 SIGTERM 掉所有在跑的无头子进程组,等于为了让第 N 个
    /// agent 上线去打断另外 N-1 个正在干活的 agent,还连带丢掉全部内存态。
    ReloadAgents,
    /// 只读:daemon 自报运行时快照(在跑的轮 + 进程还活着的无头会话 + 自己起来多久),
    /// 回 `Value(RuntimeStatus)`。
    /// Owner-only(本机 CLI),无副作用故不过 operate-gate(`gate_tier` 归 None)。
    /// `agents status` / Menu Bar 要的 busy、无头会话进程存活与 uptime 只有 daemon 内存里有；
    /// 会话发现是盘上事实,CLI 自己读,不必让 daemon 代劳 —— 这样 daemon 不在时的降级是天然的,
    /// 不是补丁。
    RuntimeStatus,
    /// 复验「这一轮此刻**在本代 daemon 内存里**还活着,且 turn_id 对得上」。
    ///
    /// 主动 CLI(`preview` / `send-image` / `react` / `chats`)专用。它们各自是独立进程,只能
    /// 读到盘上的 lease 文件;而盘上文件是 best-effort 清理的——轮结束时删除失败,旧凭证就会
    /// 一直"有效",于是一个早已结束的轮仍能往群里发东西。真正的权威是 daemon 本代内存表:
    /// 换代、轮结束一律取不到。
    ///
    /// 无副作用(纯查询),故不过 operate-gate。**daemon 不可用时调用方必须 fail-closed** ——
    /// 拿不到复验就等于没有证据,不能放行。
    VerifyActiveTurn {
        alias: String,
        conversation_tag: String,
        turn_id: String,
    },
    /// 群内摘要与内部指令使用独立协议名，防止协议不匹配时公开正文或改变执行位置。
    DelegateInstructions {
        target: crate::session::group_target::GroupTarget,
        #[serde(default)]
        summary: Option<String>,
        content: String,
        /// 不设 serde 默认值：缺这个字段的请求解析失败，不会被当成一次真投递。
        dry_run: bool,
    },
    /// 为轮内 agent 的本地文件生成浏览器预览能力 URL。daemon 持有预览服务与 token 表，CLI
    /// 只提交绝对路径；不会发送飞书消息。返回 `Value(String URL)`。
    CreateFilePreview {
        alias: String,
        conversation_tag: String,
        turn_id: String,
        path: String,
    },
    /// chat 轮内 agent 向用户提问并**阻塞等答复**(`summon ask`)。无头 CLI 专用 ——
    /// claude 走 tmux 抓屏中介(`parse::interactive`),不需要也不该走这条。
    /// `turn_id` 是本轮一次性代际凭证(来自 `CurrentTurnStore`),Core 据它认领是哪一轮在问;
    /// 换代/过期的凭证一律 fail-closed,防旧会话拿陈旧身份往当前 chat 投卡片。
    /// **一次可问多题**,一张卡渲染完、一次答完(见设计不变量 5)。
    /// 返回 `Blocked(GateId)`,由 CLI 侧 `wait_gate` 阻塞到人提交/全部跳过。
    ChatAsk {
        turn_id: String,
        alias: String,
        /// 稳定会话标签,与 turn_id 一起定位「哪个 agent 的哪个会话的哪一轮」。
        conversation_tag: String,
        questions: Vec<AskQuestion>,
    },
    /// Claude 的 PermissionRequest hook 把一次授权请求 / AskUserQuestion 交给 summon,
    /// **阻塞等人裁决**。`alias` 与 `session` 是 summon 启动该会话时写进 settings 的参数,
    /// 不来自 hook 进程环境;Core 只认这个会话 `.meta` 记的 chat。Owner-only(本机进程)。
    /// 返回 `Blocked(GateId)`;裁决前 control 连接断开即取消闸门、卡片作废。
    ClaudeHookAsk {
        alias: String,
        session: String,
        request: ClaudeHookRequest,
    },
    /// `summon dashboard open`:给本机 owner 签一个一次性本机码(60 秒、用一次即作废),回
    /// `Value(String URL)`。Owner-only:control.sock 只有本机用户连得上,这一步本身就证明了是
    /// 本机 owner;飞书身份进控制台只能走 `/dash` 私聊给发起人的一次性链接。
    DashboardOpen,
    /// Claude 的 PostCompact hook 报告这个会话刚压缩完上下文:往该会话所属的 chat 推压缩后的
    /// 上下文长度卡。`alias` 与 `session` 同 [`Intent::ClaudeHookAsk`],来自 summon 写进 settings
    /// 的参数;`trigger` 是 hook 输入里的 `manual` / `auto`,只用于日志。Owner-only(本机进程),
    /// 立即返回,不等卡片发出。
    ClaudeCompacted {
        alias: String,
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },
}

/// hook 输入里 summon 用得上的部分。`tool_input` 原样保留:卡片要展示完整入参,
/// AskUserQuestion 的题面也在里面。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeHookRequest {
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    /// 子 agent 发起时才有;主 agent 为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

/// 一道要问用户的题。`options` 为空 = 开放式(渲染成自由文本输入)。
/// `header` 是可选的短标题，与完整题干分行展示。
/// `descriptions[i]` 是 `options[i]` 的补充说明；缺项或空串都表示该选项没有说明。
/// `allow_other` 只对选项题生效：下拉旁再展示一个自由输入，非空自定义答案优先。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    #[serde(alias = "q")]
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descriptions: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_other: bool,
    /// 可多选(只对选项题生效)。答案是选中项按原顺序用 `", "` 连接 —— Claude AskUserQuestion
    /// 对多选答案的约定。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi_select: bool,
}

/// 一道题的答复。`answer=None` = 用户没答这题(允许只答一部分,见不变量 7),
/// agent 据此自行判断,而不是被逼着二选一。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    #[serde(rename = "q")]
    pub question: String,
    #[serde(rename = "a")]
    pub answer: Option<String>,
}

/// 写命令所需的权限档位(operate-gate 用)。闸门 resolve/取消是**破坏性动作**——它替 agent
/// 拍板并放它继续跑,故走 operate 名单(operators 优先、回退 allowed_users、群 fail-closed)。
/// 其余 Intent(chat 入站走自己的白名单、Owner-only 的本机命令)归 `None` = 本档不在此鉴权。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateTier {
    /// 需 operate 名单(`can_operate`)。
    Operate,
}

impl Intent {
    /// 该写意图过 operate-gate 时需要哪档名单;不在本档鉴权范围内的返回 None。
    /// 鉴权分派集中在这一处,Core 不再各自硬编码第二份判据。
    pub fn gate_tier(&self) -> Option<GateTier> {
        match self {
            Intent::QuestionResolve(..) | Intent::CancelGate(_) => Some(GateTier::Operate),
            _ => None,
        }
    }
}

/// 一条命令 = 谁 + 想干啥。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub actor: Actor,
    pub intent: Intent,
}

/// 命令处理结果。阻塞类(闸门)返回 `Blocked(GateId)` 由调用方等 resolve。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandOutcome {
    Done,
    Blocked(GateId),
    Value(serde_json::Value),
    Denied(String),
    /// The addressed gate has already ended; this is separate from authorization refusal.
    #[serde(rename = "Denied", skip_deserializing)]
    GateClosed(String),
}

/// 出站统一事件:会话 / 闸门状态变更。
#[derive(Clone, Debug)]
pub enum CoreEvent {
    Gate(GateId, GateView),
    SessionChanged(SessionView),
}

/// 闸门的来源:chat 轮内的 `summon ask`。鉴权权威 agent 就是发起提问的那个;
/// `chat_id`/`chat_type` 决定提问卡与提醒卡发往哪个会话。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateOrigin {
    pub alias: String,
    pub chat_id: String,
    pub chat_type: String,
    /// 提问者 open_id:提问卡 @ 他,提醒卡也 @ 他。
    pub requester_open_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateView {
    pub origin: GateOrigin,
    pub kind: String,
    pub text: String,
}

/// 当前仍可 resolve 的闸门快照。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingGateView {
    pub gate_id: GateId,
    pub gate: GateView,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub key: String,
    pub alias: String,
    pub live: bool,
    /// 该 agent 的 CLI（claude/cursor/codex）。
    pub cli: String,
    /// 当前 model(agent.yaml 顶层 per-CLI token;None=CLI 默认)。
    pub model: Option<String>,
    /// 当前 effort(规范逻辑档位;None=默认)。
    pub effort: Option<String>,
    /// 可选 model 候选（`messaging.models_by_cli` 分桶，该 CLI 的）。
    pub models: Vec<String>,
    /// 可选 effort 候选(`messaging.efforts` ∩ 该 CLI legal;cursor/Other 为空 → 前端不渲染 effort 行)。
    pub efforts: Vec<String>,
}

/// 写意图入口：所有适配器经此投 Command。可阻塞（闸门）。
pub trait CommandSink {
    fn submit(&self, cmd: Command) -> CommandOutcome;
}

/// 读快照入口：CLI 等本机适配器拉状态。
pub trait QueryApi {
    fn list_sessions(&self) -> Vec<SessionView>;
    fn list_gates(&self) -> Vec<PendingGateView>;
}

/// 订阅实时事件：飞书卡片 patcher、agents tail、events.jsonl 都是订阅者。
pub trait EventBus {
    fn subscribe(&self) -> std::sync::mpsc::Receiver<CoreEvent>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_serde_roundtrips() {
        let i = Intent::QuestionResolve(GateId::new("g1"), "A".into());
        let s = serde_json::to_string(&i).unwrap();
        let back: Intent = serde_json::from_str(&s).unwrap();
        assert_eq!(i, back);
    }

    #[test]
    fn delegate_intent_without_dry_run_flag_is_rejected() {
        let i = Intent::DelegateInstructions {
            target: "oc_x".into(),
            summary: None,
            content: "任务".into(),
            dry_run: true,
        };
        let back: Intent = serde_json::from_str(&serde_json::to_string(&i).unwrap()).unwrap();
        assert_eq!(i, back);
        assert!(serde_json::to_value(&i)
            .unwrap()
            .get("DelegateInstructions")
            .is_some());
        let missing_summary =
            r#"{"DelegateInstructions":{"target":"oc_x","content":"任务","dry_run":true}}"#;
        let back: Intent = serde_json::from_str(missing_summary).unwrap();
        assert!(matches!(
            back,
            Intent::DelegateInstructions { summary: None, .. }
        ));
        let legacy = r#"{"Delegate":{"target":"oc_x","content":"任务","dry_run":true}}"#;
        assert!(serde_json::from_str::<Intent>(legacy).is_err());
        let private = r#"{"DelegatePrivate":{"target":"oc_x","content":"任务","dry_run":true}}"#;
        assert!(serde_json::from_str::<Intent>(private).is_err());
        let missing = r#"{"DelegateInstructions":{"target":"oc_x","content":"任务"}}"#;
        assert!(serde_json::from_str::<Intent>(missing).is_err());
    }

    #[test]
    fn teardown_chat_intent_serde_roundtrips() {
        // 两个 mode 都过一遍 serde,确保 control.sock 上的 Command 能原样往返。
        for mode in [TeardownMode::Clean, TeardownMode::Reset] {
            let i = Intent::TeardownChat {
                alias: "starlight".into(),
                chat_id: ChatId::new("oc_abc"),
                mode,
            };
            let s = serde_json::to_string(&i).unwrap();
            let back: Intent = serde_json::from_str(&s).unwrap();
            assert_eq!(i, back);
        }
    }

    #[test]
    fn teardown_chat_is_not_gated_by_operate_list() {
        // TeardownChat 是 Owner-actor 的 CLI 命令(本机可信,无需 operate-gate)。
        let i = Intent::TeardownChat {
            alias: "a".into(),
            chat_id: ChatId::new("oc"),
            mode: TeardownMode::Clean,
        };
        assert_eq!(i.gate_tier(), None);
    }

    #[test]
    fn end_chat_session_intent_roundtrips_and_is_not_operate_gated() {
        for archive in [false, true] {
            let i = Intent::EndChatSession {
                alias: "starlight".into(),
                chat_id: ChatId::new("oc_abc"),
                archive,
            };
            let back: Intent = serde_json::from_str(&serde_json::to_string(&i).unwrap()).unwrap();
            assert_eq!(i, back);
            assert_eq!(i.gate_tier(), None);
        }
    }

    #[test]
    fn dashboard_open_intent_roundtrips_and_is_not_operate_gated() {
        let i = Intent::DashboardOpen;
        let back: Intent = serde_json::from_str(&serde_json::to_string(&i).unwrap()).unwrap();
        assert_eq!(i, back);
        assert_eq!(i.gate_tier(), None, "Owner-only,不过 operate 名单");
    }

    /// 闸门 resolve/取消是唯一走 operate 名单的意图;其余(chat 入站、Owner-only 本机命令)
    /// 不在本档。这条钉住「档位只有一处判据」,防有人再把鉴权散回各 handler。
    #[test]
    fn gate_tier_classifies_only_gate_resolutions_as_operate() {
        assert_eq!(
            Intent::QuestionResolve(GateId::new("g1"), "A".into()).gate_tier(),
            Some(GateTier::Operate)
        );
        assert_eq!(
            Intent::CancelGate(GateId::new("g1")).gate_tier(),
            Some(GateTier::Operate)
        );
        assert_eq!(
            Intent::ChatMessage {
                chat: ChatId::new("oc"),
                text: "x".into()
            }
            .gate_tier(),
            None
        );
        assert_eq!(Intent::RuntimeStatus.gate_tier(), None);
        assert_eq!(
            Intent::ChatAsk {
                turn_id: "t".into(),
                alias: "a".into(),
                conversation_tag: "g1".into(),
                questions: vec![AskQuestion {
                    question: "选哪个?".into(),
                    options: vec![],
                    ..Default::default()
                }],
            }
            .gate_tier(),
            None
        );
    }

    /// `summon ask` 的 actor 走 control.sock 原样往返:提问身份(turn_id/alias/来源消息)
    /// 一个字段都不能在线上丢,否则 Core 认不出是哪一轮在问。
    #[test]
    fn chat_agent_ask_command_serde_keeps_turn_identity() {
        let cmd = Command {
            actor: Actor::ChatAgent(Box::new(ChatAgentIdentity {
                turn_id: "turn-123".into(),
                alias: "ambrose".into(),
                requester_open_id: "ou_requester".into(),
                source_ref: MsgRef {
                    delivery_scope: "cli_app".into(),
                    chat_id: "oc_chat".into(),
                    chat_type: "group".into(),
                    message_id: "om_message".into(),
                    thread_id: None,
                    root_id: None,
                },
                chat_type: "group".into(),
                conversation_tag: "g123".into(),
            })),
            intent: Intent::ChatAsk {
                turn_id: "turn-123".into(),
                alias: "ambrose".into(),
                conversation_tag: "g123".into(),
                questions: vec![AskQuestion {
                    question: "要继续吗?".into(),
                    options: vec!["是".into(), "否".into()],
                    ..Default::default()
                }],
            },
        };
        let encoded = serde_json::to_string(&cmd).unwrap();
        let decoded: Command = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, cmd);
        assert_eq!(decoded.intent.gate_tier(), None);
    }

    #[test]
    fn ask_question_serde_accepts_legacy_and_rich_shapes() {
        let legacy: AskQuestion = serde_json::from_value(serde_json::json!({
            "q": "要继续吗?",
            "options": ["是", "否"]
        }))
        .unwrap();
        assert_eq!(legacy.question, "要继续吗?");
        assert_eq!(legacy.header, None);
        assert_eq!(legacy.descriptions, Vec::<String>::new());
        assert!(!legacy.allow_other);
        let legacy_encoded = serde_json::to_value(&legacy).unwrap();
        assert!(legacy_encoded.get("header").is_none());
        assert!(legacy_encoded.get("descriptions").is_none());
        assert!(legacy_encoded.get("allow_other").is_none());

        let rich: AskQuestion = serde_json::from_value(serde_json::json!({
            "q": "选一个发布策略",
            "header": "发布策略",
            "options": ["灰度", "全量"],
            "descriptions": ["先覆盖 5% 用户", "立即覆盖全部用户"],
            "allow_other": true
        }))
        .unwrap();
        assert_eq!(rich.header.as_deref(), Some("发布策略"));
        assert_eq!(rich.descriptions, ["先覆盖 5% 用户", "立即覆盖全部用户"]);
        assert!(rich.allow_other);

        let encoded = serde_json::to_value(&rich).unwrap();
        let decoded: AskQuestion = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, rich);
    }
}

#[cfg(test)]
mod localization_tests {
    use super::CommandOutcome;

    #[test]
    fn closed_gates_keep_the_existing_denial_wire_contract() {
        let closed = CommandOutcome::GateClosed("already handled".into());
        let wire = serde_json::to_value(&closed).unwrap();
        assert_eq!(wire, serde_json::json!({"Denied": "already handled"}));
        assert_eq!(
            serde_json::from_value::<CommandOutcome>(wire).unwrap(),
            CommandOutcome::Denied("already handled".into())
        );
    }
}
