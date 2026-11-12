//! 回放执行器 + RR1 端到端回放保真测(**整模块 `#[cfg(test)]`** —— 只在 `cargo test` 编译,
//! release/debug 二进制都不含)。
//!
//! `summon` 是纯 bin crate(无 lib target),外部 `tests/` 够不着内部 `Executor`/`Subprocess`,故
//! 把 `ReplayExecutor` 与回放测放进 crate 内的 `#[cfg(test)]` 模块——既能用全部内部类型,又是测试专属。
//!
//! 回放原理(RR-D2 Executor 级保真):`ReplayExecutor::spawn` 先把请求 spec 与录制 `spawn.json` 比对
//! (不符即 Err,防夹具漂移),再 spawn `cat <turn_dir>/stdout.raw` 出**真 Child** → 交 `Subprocess`
//! 经**真实解析管线**(`parse_stream_json_line`→`AgentMessage`)消费。故回放测的是真 parse/capture,
//! 不是预解析后的回放。

// 再加 `debug_assertions`:本模块用的 `record_store`/`RecordingExecutor`/`OutboundRecord` 全是
// debug-only 录制设施,release 下不存在,否则 `--release --all-targets` 编不过。
#![cfg(all(test, debug_assertions))]

use std::path::{Path, PathBuf};
use std::process::Child;

use crate::runtime::executor::{Executor, SpawnSpec};

/// 回放执行器:从一个录制好的 turn 目录(含 `spawn.json` + `stdout.raw`)重放子进程 stdout。
pub struct ReplayExecutor {
    turn_dir: PathBuf,
}

impl ReplayExecutor {
    pub fn new(turn_dir: impl Into<PathBuf>) -> Self {
        Self {
            turn_dir: turn_dir.into(),
        }
    }
}

impl Executor for ReplayExecutor {
    /// 比对 program/args 与录制 spec(防夹具漂移),再 spawn `cat <turn_dir>/stdout.raw` 出真 Child。
    fn spawn(&self, spec: &SpawnSpec) -> anyhow::Result<Child> {
        use summon_core::record::CassetteTurn;
        let spawn_json = self.turn_dir.join("spawn.json");
        let json = std::fs::read_to_string(&spawn_json).map_err(|e| {
            anyhow::anyhow!(summon_i18n::format!("r6063106994", spawn_json.display()))
        })?;
        let recorded: CassetteTurn = serde_json::from_str(&json).map_err(|e| {
            anyhow::anyhow!(summon_i18n::format!("r6ef72bc3e3", spawn_json.display()))
        })?;
        // 请求 spec 须与录制一致(program/args),否则是夹具与代码漂移——fail-fast。
        let req = spec.to_record();
        anyhow::ensure!(
            req.program == recorded.spec.program && req.args == recorded.spec.args,
            summon_i18n::format!(
                "r5757216752",
                req.program,
                req.args,
                recorded.spec.program,
                recorded.spec.args
            )
        );
        // 经 LocalExecutor cat 录制字节出真 Child(stdin=null 防 cat 等输入)。
        let raw = self.turn_dir.join(&recorded.stdout_file);
        crate::runtime::executor::LocalExecutor.spawn(&SpawnSpec {
            program: "cat".into(),
            args: vec![raw.to_string_lossy().into_owned()],
            stdin_null: true,
            ..Default::default()
        })
    }

    fn read_file_tail(&self, _path: &Path, _from_byte: u64) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!(summon_i18n::format!("r5da8dbbc50"))
    }

    fn tmux(&self, _args: &[&str]) -> anyhow::Result<String> {
        anyhow::bail!(summon_i18n::format!("raf00b94b67"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Subprocess;
    use crate::runtime::executor::{LocalExecutor, SpawnSpec};
    use crate::runtime::record_store::RecordStore;

    /// 假 CLI:无视参数把 fixture 原样喷 stdout(与 subprocess.rs 的同名 helper 一致)。
    fn fake_cli(jsonl: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("out.jsonl");
        std::fs::write(&data, jsonl).unwrap();
        let script = dir.path().join("fakecli.sh");
        crate::test_support::write_test_executable(
            &script,
            "#!/bin/sh\ncat \"${0%/*}/out.jsonl\"\n",
        );
        (dir, script.to_string_lossy().into_owned())
    }

    /// 跑一轮 `Subprocess::drive`、把每条 `AgentMessage`(经真实解析)收成序列。返回 (events, reply, sid)。
    fn run_capture(
        exec: impl Executor,
        spec: SpawnSpec,
    ) -> (Vec<String>, Option<String>, Option<String>) {
        let mut events: Vec<String> = vec![];
        let mut src = Subprocess::new(exec);
        let out = src
            .drive(
                spec,
                &summon_core::stream_json::parse_stream_json_line,
                &mut |_b| {},
                &mut |_tool| {},
                &mut |_acc| {},
                // 把每条 typed 事件压成稳定字符串签名(kind + 关键载荷),用于序列比对。
                &mut |ev| events.push(event_signature(ev)),
                &mut |_killer| {},
                &mut |_sid| {},
            )
            .unwrap();
        (events, out.reply, out.session_id)
    }

    /// 把一条 AgentMessage 投影成稳定签名:kind + text/tool/session_id(同一序列断言「同一 AgentEvent
    /// 序列」)。比 Debug 更抗无关字段变动,又足以捕获 parse/capture 行为漂移。
    fn event_signature(ev: &summon_core::agent_msg::AgentMessage) -> String {
        format!(
            "{:?}|text={:?}|tool={:?}|sid={:?}|partial={}",
            ev.kind, ev.text, ev.tool, ev.session_id, ev.partial
        )
    }

    /// RR1.6:`ReplayExecutor` 喂录制 stdout 跑真解析,得到的 AgentEvent 序列 == 直接跑原假 CLI。
    #[test]
    fn replay_executor_reproduces_events_vs_live_fake_cli() {
        let pong = include_str!("../../../summon-core/tests/fixtures/claude-stream-pong.jsonl");
        let (_d, prog) = fake_cli(pong);

        // 基线:直接跑假 CLI(真起 + 真解析)。
        let live_spec = SpawnSpec {
            program: prog.clone(),
            args: vec!["x".into()],
            stdin_null: true,
            ..Default::default()
        };
        let (live_events, live_reply, live_sid) = run_capture(LocalExecutor, live_spec);

        // 手造一个 cassette turn:spawn.json(program/args 与上面一致)+ stdout.raw(fixture 字节)。
        let tmp = tempfile::tempdir().unwrap();
        let store = RecordStore::new(tmp.path());
        let turn_dir = store.start_turn("scn", 0).unwrap();
        store
            .write_spawn(
                &turn_dir,
                &summon_core::record::CassetteTurn {
                    spec: summon_core::record::SpawnSpecRecord {
                        program: prog.clone(),
                        args: vec!["x".into()],
                        work_dir: None,
                        stdin_null: true,
                        env: vec![],
                    },
                    stdout_file: "stdout.raw".into(),
                },
            )
            .unwrap();
        std::fs::write(turn_dir.join("stdout.raw"), pong).unwrap();

        // 回放:同样的请求 spec(program/args 须匹配录制),经 ReplayExecutor 跑真解析。
        let replay_spec = SpawnSpec {
            program: prog,
            args: vec!["x".into()],
            stdin_null: true,
            ..Default::default()
        };
        let (replay_events, replay_reply, replay_sid) =
            run_capture(ReplayExecutor::new(&turn_dir), replay_spec);

        assert_eq!(
            replay_events, live_events,
            "回放 AgentEvent 序列应与真跑一致"
        );
        assert_eq!(replay_reply, live_reply);
        assert_eq!(replay_sid, live_sid);
        assert_eq!(replay_reply.as_deref(), Some("pong"));
    }

    /// 回放 spec 与录制 program/args 不符 → fail-fast Err(防夹具漂移)。
    #[test]
    fn replay_executor_rejects_mismatched_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecordStore::new(tmp.path());
        let turn_dir = store.start_turn("scn", 0).unwrap();
        store
            .write_spawn(
                &turn_dir,
                &summon_core::record::CassetteTurn {
                    spec: summon_core::record::SpawnSpecRecord {
                        program: "codex".into(),
                        args: vec!["exec".into()],
                        work_dir: None,
                        stdin_null: true,
                        env: vec![],
                    },
                    stdout_file: "stdout.raw".into(),
                },
            )
            .unwrap();
        std::fs::write(turn_dir.join("stdout.raw"), b"").unwrap();

        let exec = ReplayExecutor::new(&turn_dir);
        let bad = SpawnSpec {
            program: "cursor-agent".into(), // 与录制 codex 不符
            args: vec![],
            ..Default::default()
        };
        assert!(exec.spawn(&bad).is_err(), "spec 不符应 Err");
    }

    // ===== RR2.4:feishu 入站注入 + ReplayExecutor + RecordingFrontend assert 端到端 =====

    use crate::frontend::{
        ChatRef, Frontend, MsgRef, OutboundRecord, RecordSink, RecordingFrontend, StreamId,
    };
    use crate::runtime::daemon::{DispatchMsg, InboundRecord};
    use std::cell::RefCell;

    /// 最小 Frontend inner:`RecordingFrontend` 包它,turn 流程经装饰器录到出站序列(对 golden 断言)。
    /// stream_begin 返回自增 StreamId(从 1 起)。无真飞书 IO —— 回放断言只看录到的调用 + 入参。
    struct NullFrontend {
        next_id: RefCell<u64>,
    }
    impl NullFrontend {
        fn new() -> Self {
            Self {
                next_id: RefCell::new(0),
            }
        }
    }
    impl Frontend for NullFrontend {
        fn send_message(&self, _c: &ChatRef, _b: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn stream_begin(
            &self,
            _c: &ChatRef,
            _content: &str,
            _with_interrupt: bool,
        ) -> anyhow::Result<StreamId> {
            *self.next_id.borrow_mut() += 1;
            Ok(StreamId(*self.next_id.borrow()))
        }
        fn stream_patch(&self, _id: StreamId, _content: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn react(&self, _m: &MsgRef, key: &str) -> anyhow::Result<String> {
            Ok(format!("rid-{key}"))
        }
        fn unreact(&self, _m: &MsgRef, _rid: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// RR2.4:端到端回放——注入录制入站 + ReplayExecutor 喂录制 stdout 跑真解析 + RecordingFrontend
    /// assert 模式,断言出站序列 == golden `outbound.ndjson`。三件套全经**真实**接缝(入站 serde 解码、
    /// Subprocess 真解析、card_stream_consume 真流式消费器、Frontend 装饰器),不连飞书、不跑真 CLI。
    #[test]
    fn replay_feishu_inbound_to_outbound_matches_golden() {
        let pong = include_str!("../../../summon-core/tests/fixtures/claude-stream-pong.jsonl");
        let (_d, prog) = fake_cli(pong);

        // ① 录制侧:把一条入站 + 一轮 stdout cassette + golden 出站写进 tempdir(模拟真实录制产物)。
        let tmp = tempfile::tempdir().unwrap();
        let store = RecordStore::new(tmp.path());
        let scenario = "feishu-e2e";

        // 入站:一条群消息(chat_id/chat_type 决定出站卡片落哪)。经 inbound.ndjson 录回再注入。
        let DispatchMsg::Inbound(inb) = DispatchMsg::inbound_for_test("oc_e2e", "ping") else {
            unreachable!()
        };
        store
            .append_ndjson(
                scenario,
                "inbound.ndjson",
                &InboundRecord::Message {
                    app_id: "cli_app".into(),
                    inbound: inb,
                },
            )
            .unwrap();

        // turn cassette:spawn.json(program/args)+ stdout.raw(pong fixture 字节)。
        let turn_dir = store.start_turn(scenario, 0).unwrap();
        store
            .write_spawn(
                &turn_dir,
                &summon_core::record::CassetteTurn {
                    spec: summon_core::record::SpawnSpecRecord {
                        program: prog.clone(),
                        args: vec!["x".into()],
                        work_dir: None,
                        stdin_null: true,
                        env: vec![],
                    },
                    stdout_file: "stdout.raw".into(),
                },
            )
            .unwrap();
        std::fs::write(turn_dir.join("stdout.raw"), pong).unwrap();

        // ② 回放侧:注入器读回入站 → 取 chat 定位;assert Frontend 录出站;ReplayExecutor 跑真解析。
        let inbound: Vec<InboundRecord> = store.read_ndjson(scenario, "inbound.ndjson").unwrap();
        assert_eq!(inbound.len(), 1, "应注入一条入站");
        let InboundRecord::Message { inbound: inj, .. } = &inbound[0] else {
            panic!("入站应为 Message")
        };
        let agent_dir = tempfile::tempdir().unwrap();
        let chat = ChatRef {
            thread_anchor: None,
            chat_id: &inj.chat_id,
            chat_type: inj.chat_type.as_deref().unwrap_or("p2p"),
            agent_dir: agent_dir.path(),
            agent_name: "tester",
            delivery_scope: "test-scope",
        };

        let fe = RecordingFrontend::new(NullFrontend::new(), RecordSink::buffer());
        // run_source 跑**真** Subprocess 解析(经 ReplayExecutor 喂录制 stdout),把解析事件接进
        // card_stream_consume 的 on_stream/on_message —— 与生产 stream_turn_headless 同一消费器。
        let replay_spec = SpawnSpec {
            program: prog,
            args: vec!["x".into()],
            stdin_null: true,
            ..Default::default()
        };
        let (out, emitted) = crate::runtime::daemon::card_stream_consume(
            &fe,
            &chat,
            false,
            summon_core::progress::DEFAULT_MAX_STEPS,
            false,
            false, // tool_trail: 关(测试默认)
            None,
            None,
            None, // badge_seed:测试不校验角标
            // 回放承诺「不碰真实家目录」:outbox 也注入进程级 tempdir,绝不落 ~/.summon。
            &crate::runtime::outbox_store::OutboxStore::new(agent_dir.path().join("outbox")),
            &|| None,
            None, // 测试/回放:无 subagent 状态源
            &mut |_: &[String]| {},
            |on_msg, on_str, on_tl, on_ev, _on_int, _on_tick, _on_steer| {
                let mut src = Subprocess::new(ReplayExecutor::new(&turn_dir));
                src.drive(
                    replay_spec,
                    &summon_core::stream_json::parse_stream_json_line,
                    on_msg,
                    on_tl,
                    on_str,
                    on_ev,
                    &mut |_k| {},
                    &mut |_s| {},
                )
            },
        );
        assert!(out.is_ok(), "回放一轮应成功");
        assert!(emitted, "应向用户发出内容");

        // ③ 断言出站序列:on_event 的 assistant 块开出 live 卡(id=1,录制层记空 card_json ——
        // 回放前端不支持原生 CoT,开的是空壳卡)→ on_message 终态对账 stream_finish
        // (记 StreamPatch 文本 "pong")。正文能不能送达由后面那条 patch 断言。
        let recs = fe.recorded();
        match recs.first() {
            Some(OutboundRecord::StreamBeginCard {
                chat_id,
                card_json,
                id,
            }) => {
                assert_eq!(chat_id, "oc_e2e");
                assert_eq!(*id, 1);
                assert!(card_json.is_empty(), "live 卡不带正文:{card_json}");
            }
            other => panic!("首条出站应为 StreamBeginCard,实为 {other:?}"),
        }
        assert_eq!(
            recs.last(),
            Some(&OutboundRecord::SendMessage {
                chat_id: "oc_e2e".into(),
                body: "pong".into(),
            }),
            "终态对账 patch 应为 pong"
        );
    }

    // ===== RR4.3 + RR4.5:replay_scenario 端到端 harness + promote 出来的 fixture 回归 =====

    /// 端到端回放 harness:从 `tests/fixtures/scenarios/<name>/` 加载 promote 出来的 cassette
    /// (`scenario.json` + `inbound.ndjson` + `outbound.ndjson` golden + `turns/turn-<n>/`),按
    /// `ScenarioIndex` 的 flow 顺序跑**真实**管线 —— 注入入站(取 chat 定位)→ 每个 Turn 经
    /// `ReplayExecutor` 喂录制 stdout 跑真 Subprocess 解析 → `card_stream_consume` 真流式消费器经
    /// `RecordingFrontend` 录出站 → 断言出站序列 == golden。**不连飞书、不跑真 CLI、不碰真实家目录**。
    ///
    /// 诚实边界:harness 注入化(不经 daemon 的 dispatcher 线程 / Core 路由 / tmux),直接驱动
    /// `card_stream_consume`(无头与 Transcript 共享的流式消费器,即生产 `stream_turn_headless` 的
    /// 消费侧)+ 真 Subprocess 解析。故覆盖「入站 serde 解码 → 真 parse/capture → 真流式刷卡接缝 →
    /// 出站 golden」整条;**未**覆盖 dispatcher 的 chat 分线程 / operate-gate / resume 决策(那些有
    /// 各自单测)。这与 spec「回放只替换最外层 Executor/Frontend + 注入入站」一致。
    fn replay_scenario(scenario_dir: &std::path::Path) {
        use crate::frontend::{RecordSink, RecordingFrontend};
        use crate::runtime::daemon::InboundRecord;
        use summon_core::record::{FlowStep, ScenarioIndex};

        // ① 加载 scenario.json(e2e 流索引)。
        let idx_json =
            std::fs::read_to_string(scenario_dir.join("scenario.json")).expect("读 scenario.json");
        let index: ScenarioIndex = serde_json::from_str(&idx_json).expect("解析 scenario.json");

        // ② 读回各侧原子记录(入站 / 出站 golden)。turns 按 FlowStep.index 现取。
        let store = RecordStore::new(scenario_dir.parent().unwrap());
        let sub = scenario_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let inbound: Vec<InboundRecord> = store.read_ndjson(&sub, "inbound.ndjson").unwrap();
        let golden: Vec<crate::frontend::OutboundRecord> =
            store.read_ndjson(&sub, "outbound.ndjson").unwrap();

        // 回放期 agent_dir(append_transcript 落盘用):进程级 tempdir,绝不碰真实 ~/.summon。
        let agent_dir = tempfile::tempdir().unwrap();
        // 每条 flow 跑一遍;出站录到同一 buffer,最后整体对 golden。
        let fe = RecordingFrontend::new(NullFrontend::new(), RecordSink::buffer());
        for flow in &index.flows {
            for step in &flow.steps {
                match step {
                    FlowStep::Inbound { index } => {
                        // 注入:取该入站的 chat 定位(经真 serde 解码回 Inbound)。
                        let InboundRecord::Message { inbound: inj, .. } = &inbound[*index] else {
                            panic!("本 fixture 入站应为 Message");
                        };
                        assert_eq!(inj.chat_id, flow.chat_id, "入站 chat 应与 flow 一致");
                    }
                    FlowStep::Turn { index } => {
                        // 该轮的 cassette 目录,经 ReplayExecutor 喂录制 stdout 跑真解析。
                        let turn_dir = scenario_dir.join("turns").join(format!("turn-{index}"));
                        let recorded = store.load_turn(&sub, *index).unwrap();
                        let chat = ChatRef {
                            thread_anchor: None,
                            chat_id: &flow.chat_id,
                            chat_type: "p2p",
                            agent_dir: agent_dir.path(),
                            agent_name: "replayer",
                            delivery_scope: "test-scope",
                        };
                        // 请求 spec = 录制 spec(回构),ReplayExecutor 比对一致后 cat stdout.raw。
                        let replay_spec = SpawnSpec::from_record(&recorded.spec);
                        let (out, emitted) = crate::runtime::daemon::card_stream_consume(
                            &fe,
                            &chat,
                            false,
                            summon_core::progress::DEFAULT_MAX_STEPS,
                            false,
                            false, // tool_trail: 关(测试默认)
                            None,
                            None,
                            None, // badge_seed:测试不校验角标
                            // 回放承诺「不碰真实家目录」:outbox 注入进程级 tempdir。
                            &crate::runtime::outbox_store::OutboxStore::new(
                                agent_dir.path().join("outbox"),
                            ),
                            &|| None,
                            None, // 回放:无 subagent 状态源
                            &mut |_: &[String]| {},
                            |on_msg, on_str, on_tl, on_ev, _on_int, _on_tick, _on_steer| {
                                let mut src = Subprocess::new(ReplayExecutor::new(&turn_dir));
                                src.drive(
                                    replay_spec,
                                    &summon_core::stream_json::parse_stream_json_line,
                                    on_msg,
                                    on_tl,
                                    on_str,
                                    on_ev,
                                    &mut |_k| {},
                                    &mut |_s| {},
                                )
                            },
                        );
                        assert!(out.is_ok(), "回放一轮应成功");
                        assert!(emitted, "应向用户发出内容");
                    }
                    FlowStep::Outbound { .. } => { /* 出站由上面录进 fe,末尾整体断言 */
                    }
                }
            }
        }

        // ③ 断言:录到的出站序列 == golden outbound(逐条)。
        assert_eq!(fe.recorded(), golden, "回放出站序列应与 golden 逐条一致");
    }

    /// RR4.5:promote 出来的 fixture `cursor-pong` 经 `replay_scenario` 回归 —— `cargo test` 可跑、
    /// 不需真飞书/真 CLI。验「入站 ping → 无头一轮回放出 pong → 出站卡片 == golden」整条管线。
    #[test]
    fn replay_scenario_cursor_pong_matches_golden() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/scenarios/cursor-pong");
        replay_scenario(&dir);
    }
}
