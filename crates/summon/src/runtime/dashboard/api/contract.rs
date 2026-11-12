//! 前后端契约:为每个方法产出一份样例 `{params, data}`,写进 `dashboard/src/api/fixtures/<方法>.json`。
//! 前端测试拿这些样例逐个按 `types.ts` 校验形状 —— Rust 改了响应形状而前端没跟上,前端测试挂;
//! 样例与此刻的实现对不上,这里挂。`UPDATE_FIXTURES=1` 时重写样例。
//!
//! 样例数据全部来自测试夹具(虚构的 ID 与名字)。随时间、机器变化的值(时间戳、进程号、临时目录、
//! 随机任务号)换成固定值,样例在任何机器上逐字节相同。

use std::path::PathBuf;

use serde_json::{json, Map, Value};
use summon_core::schedule::{Kind, Schedule};
use summon_core::settings::SettingKey;
use summon_core::{ScheduleContext, ScheduleOutput};

use super::testkit::*;
use crate::runtime::daemon::dashboard_port;
use crate::runtime::dashboard::server::{self, ApiCtx};
use crate::runtime::dashboard::sessions::ChatKind;
use crate::runtime::dashboard::writer;

const FIXED_TIME: &str = "2026-10-21T09:30:00+08:00";
const FIXED_JOB: &str = "5f0c1e2d3b4a59687766554433221100";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/src/api/fixtures")
}

/// 样例环境:三个 agent(一个读不出)、两个群、几条定时任务与说明文件。
fn sample() -> Kit {
    let kit = Kit::new();
    std::fs::write(
        kit.home().join("config.yaml"),
        "# 整机配置\nmessaging:\n  cot_visible: true  # 显示过程\n  idle_retire_secs: 21600\n",
    )
    .unwrap();
    let starlight = kit.add_agent(
        "starlight",
        &agent_yaml(
            "starlight",
            "星光",
            "claude",
            &format!("model: claude-opus-4-8\neffort: high\naliases:\n  {PEER_UNION}: 另一位用户\nmessaging:\n  cot_tool_details: true\n"),
        )
        .replace(
            "  working_dir: /tmp\n",
            "  working_dir: /tmp\n  env:\n    HTTPS_PROXY: ${CORP_PROXY}\n    API_TOKEN: sk-example-literal\n",
        )
        .replace(
            &format!("operators: [{ME_UNION}]"),
            &format!("operators: [{ME_UNION}]\n  chat_operators:\n    {GROUP}: [{PEER_UNION}]"),
        ),
    );
    let arcadia = kit.add_agent(
        "arcadia",
        &agent_yaml("arcadia", "阿卡迪亚", "codex", "model: gpt-5.5\n")
            .replace(
                "  working_dir: /tmp\n",
                "  working_dir: /tmp\n  codex_sandbox: workspace-write\n",
            )
            .replace(
                &format!("operators: [{ME_UNION}]"),
                &format!("operators: [{PEER_UNION}]"),
            ),
    );
    let broken = kit.agents_dir().join("saturn");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(
        broken.join("agent.yaml"),
        "name: saturn\nruntime:\n  cli: claude\n   broken: [\n",
    )
    .unwrap();
    kit.add_chat(&starlight, "group", GROUP, "示例群");
    kit.add_chat(&starlight, "p2p", P2P, "");
    kit.add_chat(&arcadia, "group", GROUP, "示例群");
    let session = starlight.tmux_name_for_chat("group", GROUP, false);
    dashboard_port::update_chat_settings_if(
        &starlight,
        &session,
        &[
            (SettingKey::BadgeEffort, Some(false)),
            (SettingKey::CotThinking, Some(true)),
        ],
        |_| true,
    )
    .unwrap();
    let chat_doc = starlight.dir.join("chats").join(GROUP);
    std::fs::create_dir_all(&chat_doc).unwrap();
    std::fs::write(chat_doc.join("CLAUDE.md"), "本群只谈 CI 与发布。\n").unwrap();
    std::fs::write(starlight.dir.join("SOUL.md"), "我是星光。\n").unwrap();
    std::fs::write(kit.home().join("CLAUDE.md"), "# 全局约定\n\n回复用中文。\n").unwrap();
    let store = kit.env().schedule_store();
    let schedule = |id: &str, agent: &str, kind: Kind, spec: &str, prompt: &str| Schedule {
        id: id.to_string(),
        kind,
        spec: spec.to_string(),
        agent: agent.to_string(),
        chat_id: GROUP.to_string(),
        chat_type: "group".to_string(),
        prompt: prompt.to_string(),
        created_by: ME_OPEN.to_string(),
        created_at: "2026-10-01T09:00:00+08:00".to_string(),
        output: ScheduleOutput::Result,
        context: ScheduleContext::Resume,
        enabled: true,
        last_fired: Some("2026-10-20T09:30:00+08:00".to_string()),
        revision: 0,
    };
    store
        .add(&schedule(
            "SCH-1",
            "starlight",
            Kind::Cron,
            "30 9 * * 1-5",
            "看一眼昨晚的 CI,挂了的列出来",
        ))
        .unwrap();
    store
        .add(&schedule(
            "SCH-2",
            "arcadia",
            Kind::Interval,
            "3600",
            "巡检发布队列",
        ))
        .unwrap();
    store
        .add(&schedule(
            "SCH-3",
            "starlight",
            Kind::Cron,
            "0 18 * * *",
            "汇总今天合入的改动",
        ))
        .unwrap();
    store.set_enabled("SCH-3", false).unwrap();
    kit
}

fn feishu(kit: &Kit) -> ApiCtx {
    fixed(kit.feishu(issuer("starlight", GROUP, ChatKind::Group)))
}

fn owner(kit: &Kit) -> ApiCtx {
    fixed(kit.owner())
}

/// 样例请求的时刻。
const REQUEST_TIME: &str = "2026-10-20T14:32:00+08:00";

/// 定时任务的「下次触发」按请求时刻算:钉在一个固定时刻。
fn fixed(mut ctx: ApiCtx) -> ApiCtx {
    ctx.now = chrono::DateTime::parse_from_rfc3339(REQUEST_TIME)
        .unwrap()
        .with_timezone(&chrono::Local);
    ctx
}

fn rev(path: PathBuf) -> String {
    writer::revision(std::fs::read(path).ok().as_deref())
}

/// 随机器、时刻变化的值换成固定值。
fn normalize(value: Value, kit: &Kit) -> Value {
    let tmp = kit.home().to_string_lossy().to_string();
    let private = format!("/private{tmp}");
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    fn walk(v: Value, key: Option<&str>, paths: &dyn Fn(&str) -> String) -> Value {
        match v {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| {
                        let v = walk(v, Some(&k), paths);
                        (paths(&k), v)
                    })
                    .collect::<Map<String, Value>>(),
            ),
            Value::Array(items) if key == Some("next") => {
                Value::Array(items.into_iter().map(|_| Value::from(FIXED_TIME)).collect())
            }
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|v| walk(v, None, paths)).collect())
            }
            Value::String(_)
                if matches!(
                    key,
                    Some(
                        "next_fire"
                            | "last_active"
                            | "started_at"
                            | "expires_at"
                            | "turn_started_at"
                            | "waiting_since"
                            | "start"
                            | "since"
                            | "until"
                            | "fetched_at"
                            | "generated_at"
                            | "at"
                    )
                ) =>
            {
                Value::from(FIXED_TIME)
            }
            Value::Number(_) if matches!(key, Some("started_at_ms" | "ended_at_ms" | "at_ms")) => {
                Value::from(FIXED_MS)
            }
            Value::String(_) if key == Some("job") => Value::from(FIXED_JOB),
            // 日志翻页位置里带着文件的 inode
            Value::String(_) if key == Some("before") => Value::from("1:0"),
            Value::String(_) if key == Some("version") => Value::from("0.6.18"),
            Value::String(_) if key == Some("commit") => Value::from("0000000"),
            Value::String(_) if key == Some("hostname") => Value::from("example-host"),
            Value::Number(_) if key == Some("pid") => Value::from(4242),
            Value::String(s) => Value::String(paths(&s)),
            other => other,
        }
    }
    let paths = |s: &str| {
        let s = s.replace(&private, "~/.summon").replace(&tmp, "~/.summon");
        if !home.is_empty() && s.starts_with(&home) {
            format!("~{}", &s[home.len()..])
        } else {
            s
        }
    };
    walk(value, None, &paths)
}

/// 时间戳样例值(毫秒)。
const FIXED_MS: i64 = 1_792_476_000_000;

/// 台账里一轮样例:群里一位用户问的、跑完了的一轮;价格表收录了它主要用的模型,子 agent 用的那个
/// 没收录。这一轮结束之后,后台子 agent 又跑了一阵,记成一段轮外用量。
fn sample_turn(kit: &Kit) {
    use summon_core::turn_ledger::{
        ModelUsage, TurnCtx, TurnEnd, TurnOrigin, TurnQuota, TurnRecord,
    };
    // 台账按请求时刻往前查:这一轮结束在请求之前几秒。
    let now = chrono::DateTime::parse_from_rfc3339(REQUEST_TIME)
        .unwrap()
        .timestamp_millis();
    let record = TurnRecord {
        agent: "starlight".into(),
        chat_id: GROUP.into(),
        tag: summon_core::registry::chat_tag("group", GROUP),
        turn: Some("00000000-0000-4000-8000-000000000001".into()),
        origin: TurnOrigin::Human,
        schedule: None,
        requester: Some(ME_OPEN.into()),
        message_id: "om_00000000000000000000000000000001".into(),
        prompt_head: "帮我看下 CI 为什么挂了".into(),
        started_at_ms: now - 95_000,
        ended_at_ms: now - 5_000,
        end: TurnEnd::Done,
        error: None,
        reattached: false,
        tool_steps: 6,
        cli: Some("claude".into()),
        model: Some("claude-opus-5-5".into()),
        effort: Some("high".into()),
        ctx: Some(TurnCtx {
            used: Some(86_000),
            window: Some(200_000),
            pct: Some(43),
        }),
        usage: vec![
            ModelUsage {
                model: "claude-opus-5-5".into(),
                band: 0,
                variant: None,
                requests: 12,
                input: 2_400,
                output: 18_000,
                cache_read: 1_200_000,
                cache_write_5m: 0,
                cache_write_1h: 64_000,
            },
            ModelUsage {
                model: "composer-2".into(),
                requests: 3,
                input: 900,
                output: 1_500,
                cache_read: 20_000,
                ..Default::default()
            },
        ],
        quota: Some(TurnQuota {
            five_hour_left: Some(56),
            five_hour_resets_at: None,
            long_left: Some(83),
            long_resets_at: None,
        }),
        feed: Some("修好单测断言".into()),
    };
    let dir = kit.home().join("run/turns/starlight");
    std::fs::create_dir_all(&dir).unwrap();
    let mut line = serde_json::to_string(&record).unwrap();
    line.push('\n');
    std::fs::write(dir.join(summon_core::turn_ledger::file_name(now)), line).unwrap();
    let outside = summon_core::turn_ledger::OutsideUsage {
        agent: "starlight".into(),
        chat_id: GROUP.into(),
        tag: summon_core::registry::chat_tag("group", GROUP),
        cli: "claude".into(),
        from_ms: now - 4_000,
        to_ms: now - 2_000,
        usage: vec![ModelUsage {
            model: "claude-opus-5-5".into(),
            requests: 2,
            input: 30,
            output: 2_000,
            cache_read: 90_000,
            cache_write_1h: 4_000,
            ..Default::default()
        }],
    };
    crate::runtime::turn_ledger::append_outside(
        &crate::runtime::turn_ledger::outside_dir_in(&kit.home().join("run")),
        &outside,
    )
    .unwrap();
    crate::runtime::prices::refresh(&kit.home().join("run"), FIXED_MS, || {
        Ok(
            r#"{"claude-opus-5-5": {"input_cost_per_token": 4e-06, "output_cost_per_token": 2e-05,
            "cache_read_input_token_cost": 2e-07, "cache_creation_input_token_cost": 5e-06,
            "cache_creation_input_token_cost_above_1hr": 8e-06}}"#
                .into(),
        )
    })
    .unwrap();
}

/// 洞察页样例:请求前一天傍晚,本群四轮(间隔 1 分钟、30 分钟、2 小时)、另一个群两轮
/// (间隔 65 分钟)。时刻取在 +08 的 17–21 点(UTC 的 9–13 点),当日口径与日期在两种时区下都相同,
/// 样例逐字节不变。
fn sample_insights(kit: &Kit) {
    use summon_core::turn_ledger::{TurnEnd, TurnOrigin, TurnRecord};
    let at = |hm: &str| {
        chrono::DateTime::parse_from_rfc3339(&format!("2026-10-19T{hm}:00+08:00"))
            .unwrap()
            .timestamp_millis()
    };
    let other = "oc_00000000000000000000000000000004";
    let dir = kit.home().join("run/turns/starlight");
    std::fs::create_dir_all(&dir).unwrap();
    let mut text = String::new();
    for (chat_id, start, end, head) in [
        (GROUP, "17:00", "17:02", "帮我看下 CI 为什么挂了"),
        (GROUP, "17:03", "17:10", "继续"),
        (other, "17:20", "17:25", "整理一下本周的发布记录"),
        (GROUP, "17:40", "17:45", "进展如何?"),
        (other, "18:30", "18:35", "好的"),
        (
            GROUP,
            "19:45",
            "19:50",
            "把发布脚本的超时改成可配置并补测试",
        ),
    ] {
        let record = TurnRecord {
            agent: "starlight".into(),
            chat_id: chat_id.into(),
            tag: summon_core::registry::chat_tag("group", chat_id),
            turn: None,
            origin: TurnOrigin::Human,
            schedule: None,
            requester: Some(ME_OPEN.into()),
            message_id: "om_00000000000000000000000000000001".into(),
            prompt_head: head.into(),
            started_at_ms: at(start),
            ended_at_ms: at(end),
            end: TurnEnd::Done,
            error: None,
            reattached: false,
            tool_steps: 0,
            cli: Some("claude".into()),
            model: None,
            effort: None,
            ctx: None,
            usage: Vec::new(),
            quota: None,
            feed: None,
        };
        text.push_str(&serde_json::to_string(&record).unwrap());
        text.push('\n');
    }
    std::fs::write(
        dir.join(summon_core::turn_ledger::file_name(at("17:00"))),
        text,
    )
    .unwrap();
}

/// 洞察分析样例:主 agent 显式配成星光,这段时间已有一份分析(不起进程);可带一次没成功的重新生成。
fn sample_analysis(kit: &Kit, retry_failed: Option<&str>) {
    sample_insights(kit);
    let path = kit.home().join("config.yaml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("primary_agent: starlight\n");
    std::fs::write(&path, text).unwrap();
    super::insights::seed_analysis(
        &owner(kit),
        json!({"range": "7d", "lang": "zh-CN"}),
        "- 观察:15 分钟以上的间隔只占 3 条,却占了间隔总时长的大部分。\n\
         - 建议:把需要确认的事项集中在回复末尾,减少来回。\n\
         - 局限:样本不足 30 条,结论仅供参考。",
        FIXED_MS,
        retry_failed,
    );
}

fn wait_cleanup(ctx: &ApiCtx, id: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let v = super::cleanup::status(ctx, json!({"job":id})).unwrap();
        if !matches!(v["status"].as_str(), Some("preparing" | "cleaning")) {
            return v;
        }
        assert!(std::time::Instant::now() < deadline, "cleanup timeout: {v}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
fn cleanup_target(kit: &Kit) -> ApiCtx {
    kit.add_agent("retired", &agent_yaml("retired", "示例", "codex", ""));
    std::fs::write(
        kit.agents_dir().join("retired/.env"),
        "DASH_TEST_RETIRED_APP_ID=cli_0000000000000099\nDASH_TEST_RETIRED_APP_SECRET=s\n",
    )
    .unwrap();
    let ctx = kit.owner();
    super::agents::set_enabled(&ctx, json!({"agent":"retired", "enabled":false})).unwrap();
    ctx
}
fn cleanup_ready(kit: &Kit) -> (ApiCtx, String) {
    let ctx = cleanup_target(kit);
    let v = super::cleanup::prepare(&ctx, json!({"agent":"retired"})).unwrap();
    let id = v["job"].as_str().unwrap().to_string();
    assert_eq!(wait_cleanup(&ctx, &id)["status"], "ready");
    (ctx, id)
}

/// 每个方法:在一份新的样例环境里,以给定身份带给定参数调一次。
fn cases() -> Vec<(&'static str, Value, Value)> {
    let mut out = Vec::new();
    let mut push = |method: &'static str, build: &dyn Fn(&Kit) -> (ApiCtx, Value)| {
        let kit = sample();
        let (ctx, params) = build(&kit);
        let registry = server::registry();
        let handler = registry
            .handler(method)
            .unwrap_or_else(|| panic!("{method} 没登记"));
        let mut data = handler(&ctx, params.clone()).unwrap_or_else(|e| {
            panic!(
                "{method} 样例调用失败:{} {}",
                serde_json::to_string(&e.code).unwrap(),
                e.message
            )
        });
        if method == "attention.list" {
            data["observed_at"] = json!(FIXED_TIME);
            for item in data["items"].as_array_mut().unwrap() {
                item["registered_at"] = json!(FIXED_TIME);
            }
        }
        if method.starts_with("agents.cleanup.") {
            if let Some(id) = data["job"].as_str() {
                wait_cleanup(&ctx, id);
            }
            // 压缩包含临时 inode/mtime；契约只保留块下载的形状，内容另有逐字节回归。
            if data["bytes"].is_number() {
                data["bytes"] = json!(1024);
            }
            if data["sha256"].is_string() {
                data["sha256"] = json!("0123456789abcdef".repeat(4));
            }
            if data.get("base64").is_some() {
                data["base64"] = json!("ZXhhbXBsZQ==");
                data["next"] = json!(7);
            }
        }
        out.push((method, normalize(params, &kit), normalize(data, &kit)));
    };
    let agent_yaml_rev = |kit: &Kit| rev(kit.agents_dir().join("starlight/agent.yaml"));
    push("session.whoami", &|kit| (feishu(kit), json!({})));
    push("status.get", &|kit| (feishu(kit), json!({})));
    push("status.issues", &|kit| (owner(kit), json!({})));
    push("config.schema", &|kit| (feishu(kit), json!({})));
    push("config.get", &|kit| {
        (
            feishu(kit),
            json!({"scope": {"level": "agent", "agent": "starlight"}}),
        )
    });
    push("config.patch", &|kit| {
        (
            owner(kit),
            json!({"scope": {"level": "global"}, "revision": rev(kit.home().join("config.yaml")), "changes": [
                {"key": "messaging.badge_effort", "value": false},
                {"key": "dashboard.port", "value": 7800},
            ]}),
        )
    });
    push("config.raw.get", &|kit| {
        (feishu(kit), json!({"file": "agent", "agent": "starlight"}))
    });
    push("config.raw.check", &|kit| {
        (
            feishu(kit),
            json!({"file": "global", "text": "messaging:\n  cot_visable: false\n"}),
        )
    });
    push("config.raw.put", &|kit| {
        (
            owner(kit),
            json!({"file": "global", "revision": rev(kit.home().join("config.yaml")),
                   "text": "# 整机配置\nmessaging:\n  cot_visible: false\n"}),
        )
    });
    push("agents.list", &|kit| (feishu(kit), json!({})));
    push("agents.get", &|kit| {
        (feishu(kit), json!({"agent": "starlight"}))
    });
    push("agents.cleanup.prepare", &|kit| {
        (cleanup_target(kit), json!({"agent":"retired"}))
    });
    push("agents.cleanup.status", &|kit| {
        let (ctx, id) = cleanup_ready(kit);
        (ctx, json!({"job":id}))
    });
    push("agents.cleanup.download", &|kit| {
        let (ctx, id) = cleanup_ready(kit);
        (ctx, json!({"job":id,"offset":0}))
    });
    push("agents.cleanup.confirm", &|kit| {
        let (ctx, id) = cleanup_ready(kit);
        let v = super::cleanup::download(&ctx, json!({"job":id,"offset":0})).unwrap();
        assert_eq!(v["done"], true);
        (ctx, json!({"job":id,"agent":"retired","saved":true}))
    });
    push("agents.set_enabled", &|kit| {
        (feishu(kit), json!({"agent": "starlight", "enabled": false}))
    });
    push("chats.list", &|kit| (feishu(kit), json!({})));
    push("chats.get", &|kit| {
        (feishu(kit), json!({ "chat_id": GROUP }))
    });
    push("chats.set_mentionless", &|kit| {
        (
            feishu(kit),
            json!({"agent": "starlight", "chat_id": GROUP, "enabled": true}),
        )
    });
    push("chats.mentionless_job", &|kit| {
        let ctx = feishu(kit);
        let job = (server::registry().handler("chats.set_mentionless").unwrap())(
            &ctx,
            json!({"agent": "starlight", "chat_id": GROUP, "enabled": true}),
        )
        .unwrap();
        (ctx, json!({"job": job["job"]}))
    });
    push("chats.set_operators", &|kit| {
        (
            feishu(kit),
            json!({"agent": "starlight", "chat_id": GROUP, "revision": agent_yaml_rev(kit),
                   "members": [PEER_UNION, "ou_00000000000000000000000000000002"]}),
        )
    });
    push("chats.set_work_dir", &|kit| {
        (
            feishu(kit),
            json!({"agent": "starlight", "chat_id": GROUP, "revision": agent_yaml_rev(kit), "mapping": "/tmp", "clear_override": false}),
        )
    });
    push("sentinel.lark_user", &|kit| {
        (owner(kit), json!({"profile": "me"}))
    });
    push("attention.list", &|kit| {
        *kit.port.attention.lock().unwrap() =
            Some(Ok(vec![crate::runtime::core::AttentionRequest {
                id: "request-example".into(),
                origin: summon_core::core_port::GateOrigin {
                    alias: "starlight".into(),
                    chat_id: GROUP.into(),
                    chat_type: "group".into(),
                    requester_open_id: ME_OPEN.into(),
                },
                source: crate::runtime::core::AttentionSource::AgentAsk,
                text: "选择发布窗口".into(),
                question_count: 2,
                waited_secs: 60,
                registered_at: FIXED_TIME.parse().unwrap(),
                position: Some("77".into()),
                applink_host: "applink.feishu.cn".into(),
            }]));
        (owner(kit), json!({}))
    });
    push("sessions.list", &|kit| {
        let mut fleet = sample_fleet();
        fleet.comment_tasks.push(summon_core::fleet::CommentTask {
            id: "comment-task".into(),
            agent: "starlight".into(),
            title: "合成文档评论".into(),
            stage: "drafting".into(),
            status: "正在起草".into(),
            status_en: "Drafting".into(),
            busy: true,
            received_ms: 1_800_000_000_000,
            updated_ms: 1_800_000_001_000,
            pid: Some(4242),
            error: None,
        });
        kit.port.set_fleet(fleet);
        mark_sample_waiting(&kit.env().agent("starlight").unwrap());
        (feishu(kit), json!({}))
    });
    push("sessions.get", &|kit| {
        kit.port.set_fleet(sample_fleet());
        sample_turn(kit);
        (feishu(kit), json!({"agent": "starlight", "chat_id": GROUP}))
    });
    push("comments.stop", &|kit| {
        let store =
            crate::runtime::document_comments::Store::new(kit.home().join("run/document-comments"));
        let agent = kit.env().agent("starlight").unwrap();
        let app = crate::runtime::daemon::resolve_creds(&agent).0.unwrap();
        let id = store
            .receive(
                crate::runtime::document_comments::CommentNotice {
                    app_id: app,
                    event_id: "event".into(),
                    file_token: "doc_test".into(),
                    file_type: "docx".into(),
                    comment_id: "100".into(),
                    reply_id: "200".into(),
                    sender_open_id: "ou_reader".into(),
                    recipient_open_id: "ou_bot".into(),
                },
                100,
                true,
            )
            .unwrap();
        (owner(kit), json!({"agent": "starlight", "id": id}))
    });
    push("sessions.act", &|kit| {
        (
            owner(kit),
            json!({"agent": "starlight", "chat_id": GROUP, "action": "exit"}),
        )
    });
    push("usage.summary", &|kit| {
        kit.port.set_fleet(sample_fleet());
        sample_turn(kit);
        (feishu(kit), json!({"range": "7d"}))
    });
    push("insights.summary", &|kit| {
        sample_insights(kit);
        (feishu(kit), json!({"range": "7d"}))
    });
    push("insights.analysis.get", &|kit| {
        sample_analysis(kit, Some("重新生成时 claude 异常退出"));
        (feishu(kit), json!({"range": "7d", "lang": "zh-CN"}))
    });
    push("insights.analysis.start", &|kit| {
        sample_analysis(kit, None);
        (owner(kit), json!({"range": "7d", "lang": "zh-CN"}))
    });
    push("logs.tail", &|kit| {
        let run = kit.home().join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(
            run.join("daemon.log"),
            "[2026-10-20 14:30:58] [summon] [starlight] 收到消息\n\
             [2026-10-20 14:31:02] [summon] [starlight] 投递失败(将重试):timeout\n\
             [2026-10-20 14:31:40] [summon] [starlight] 本轮完成\n",
        )
        .unwrap();
        (owner(kit), json!({"file": "daemon", "limit": 2}))
    });
    push("turns.list", &|kit| {
        sample_turn(kit);
        (
            feishu(kit),
            json!({"agent": "starlight", "chat_id": GROUP, "limit": 10}),
        )
    });
    push("schedules.list", &|kit| (feishu(kit), json!({})));
    push("schedules.check_when", &|kit| {
        (feishu(kit), json!({"when": "weekday 09:30"}))
    });
    push("schedules.edit", &|kit| {
        (
            feishu(kit),
            json!({"id": "SCH-1", "edit": {"prompt": "看一眼昨晚的 CI,挂了的标出负责人", "output": "progress"}}),
        )
    });
    push("schedules.set_enabled", &|kit| {
        (feishu(kit), json!({"id": "SCH-3", "enabled": true}))
    });
    push("docs.get", &|kit| {
        (
            feishu(kit),
            json!({"doc": {"level": "agent", "agent": "starlight", "file": "SOUL.md"}}),
        )
    });
    push("docs.put", &|kit| {
        let path = kit
            .agents_dir()
            .join("starlight/chats")
            .join(GROUP)
            .join("CLAUDE.md");
        (
            feishu(kit),
            json!({"doc": {"level": "chat", "agent": "starlight", "chat_id": GROUP},
                   "revision": rev(path), "text": "本群只谈 CI、发布与值班。\n"}),
        )
    });
    push("names.resolve", &|kit| {
        (
            feishu(kit),
            json!({"agent": "starlight", "ids": [GROUP, "ou_00000000000000000000000000000003"]}),
        )
    });
    push("preview.render", &|kit| {
        (
            feishu(kit),
            json!({"scope": {"level": "chat", "agent": "starlight", "chat_id": GROUP},
                   "draft": [{"key": "messaging.cot_tool_details", "value": false}]}),
        )
    });
    out
}

#[test]
fn contract_fixtures_are_current() {
    let dir = fixtures_dir();
    let update = std::env::var("UPDATE_FIXTURES").is_ok_and(|v| v == "1");
    let cases = cases();
    // 每个登记的方法都要有样例(会话两个方法在 server 里登记,也算)。
    let registry = server::registry();
    let mut methods = registry.methods();
    methods.sort_unstable();
    let mut covered: Vec<&str> = cases.iter().map(|(m, _, _)| *m).collect();
    covered.sort_unstable();
    assert_eq!(methods, covered, "每个接口都要有契约样例");
    let mut stale = Vec::new();
    for (method, params, data) in cases {
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({"params": params, "data": data})).unwrap()
        );
        assert!(
            !text.contains("sk-example-literal"),
            "{method} 的样例漏出了密钥"
        );
        let path = dir.join(format!("{method}.json"));
        if update {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &text).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            stale.push(method);
        }
    }
    assert!(
        stale.is_empty(),
        "这些接口的契约样例与实现对不上:{stale:?};确认是有意的改动后运行 \
         UPDATE_FIXTURES=1 cargo nextest run -p summon contract_fixtures_are_current 重写,并让前端跟上"
    );
}
