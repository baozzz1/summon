//! P4 工作流 IO + 执行编排。纯件(DAG 拓扑/环检测/`ready_steps`/占位)在 `summon-core::workflow`;
//! 这里读 `~/.summon/workflows/<name>.yaml` + **worker 临时 run**(`run_worker_once`)按拓扑
//! 驱动执行,`{step_id}` 注入上游产出。**绝不碰 chat 热路径**(走和 delegate 同一条 worker 路径)。

pub mod runner_transient;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use summon_core::registry::Registry;
use summon_core::workflow::{substitute_refs, Workflow};

use crate::workflow::runner_transient::run_worker_once;

/// 工作流声明存储(`<root>/workflows/<name>.yaml`)。root 可注入,便于单测。
pub struct WorkflowStore {
    root: PathBuf,
}

impl WorkflowStore {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn at_summon_home() -> Self {
        Self::new(&crate::runtime::paths::summon_home())
    }

    fn dir(&self) -> PathBuf {
        self.root.join("workflows")
    }

    /// 读一份工作流。**「文件不存在」与「YAML 解析失败」必须可区分**:旧实现两处都 `.ok()`
    /// 压成 `None`,调用方只能报「无此工作流」——而 `workflow ls` 明明列得出它,用户无从知道
    /// 真正原因是少写了必填的 `name` 字段(实测踩到)。解析失败时把 serde 的原始报错带出来。
    pub fn load(&self, name: &str) -> Result<Workflow> {
        let path = self.dir().join(format!("{name}.yaml"));
        let s = fs::read_to_string(&path)
            .with_context(|| summon_i18n::format!("r5a2ccc9ced", path.display()))?;
        serde_yaml::from_str(&s)
            .with_context(|| summon_i18n::format!("r8e2b1873d0", path.display()))
    }

    /// 列全部工作流名(按名排序)。
    pub fn list_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(self.dir()) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "yaml") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        out.push(stem.to_string());
                    }
                }
            }
        }
        out.sort();
        out
    }
}

/// 执行一份工作流:topo 校验后,按拓扑序逐步起 worker 临时 run(**v1 顺序**——`ready_steps` 已支持
/// 并行,后续增强成并发起;顺序执行结果正确,只是不并行),`{step_id}` 注入上游产出。返回每步
/// `(id, result)`。fail-closed:topo 非法 / step agent 未注册 / 某步执行失败 → 即停 Err。
pub fn run_workflow(
    wf: &Workflow,
    reg: &Registry,
    poll: u64,
    debounce: u64,
) -> Result<Vec<(String, String)>> {
    wf.topo_order()
        .map_err(|e| anyhow::anyhow!(summon_i18n::format!("rb86330d7d2")))?;
    let env: Vec<(String, String)> = Vec::new();
    let mut results: HashMap<String, String> = HashMap::new();
    let mut done: HashSet<String> = HashSet::new();
    let mut order: Vec<(String, String)> = Vec::new();
    while done.len() < wf.steps.len() {
        // ready_steps 借 wf(不借 done),取出后即可改 done。v1 取首个 ready 顺序跑。
        let step = {
            let ready = wf.ready_steps(&done);
            ready
                .first()
                .copied()
                .context(summon_i18n::text!("r412c3d9ba2"))?
                .clone()
        };
        let agent = reg
            .agents
            .iter()
            .find(|a| a.name == step.agent)
            .with_context(|| summon_i18n::format!("ra20b850178", step.id, step.agent))?;
        let prompt = substitute_refs(&step.prompt, &results);
        let out = crate::runtime::agent_availability::with_local_start(
            agent,
            &crate::runtime::paths::run_dir().join("inbox"),
            |fresh| run_worker_once(fresh, &prompt, None, &env, poll, debounce, None),
        )
        .with_context(|| summon_i18n::format!("r818097b00e", step.id))?;
        let result = out.reply.unwrap_or_default();
        results.insert(step.id.clone(), result.clone());
        done.insert(step.id.clone());
        order.push((step.id, result));
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use summon_core::workflow::Step;

    #[test]
    fn store_round_trips_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let st = WorkflowStore::new(tmp.path());
        assert!(st.list_names().is_empty());
        let wf = Workflow {
            name: "demo".into(),
            steps: vec![Step {
                id: "a".into(),
                agent: "sk".into(),
                prompt: "do a".into(),
                needs: vec![],
            }],
        };
        fs::create_dir_all(st.dir()).unwrap();
        fs::write(
            st.dir().join("demo.yaml"),
            serde_yaml::to_string(&wf).unwrap(),
        )
        .unwrap();
        assert_eq!(st.list_names(), vec!["demo".to_string()]);
        assert_eq!(st.load("demo").unwrap().steps.len(), 1);
        let missing = format!("{:#}", st.load("missing").expect_err("不存在必须报错"));
        assert!(missing.contains("无此工作流"), "{missing}");
    }

    /// 「文件不存在」与「YAML 坏了」必须给出可区分的错误:`ls` 列得出但 `show` 报「无此工作流」
    /// 会把用户引向错误方向(实测:少写必填的 `name` 就会这样)。钉住报错要点出解析失败 + 必填字段。
    #[test]
    fn load_distinguishes_parse_failure_from_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let st = WorkflowStore::new(tmp.path());
        fs::create_dir_all(st.dir()).unwrap();
        // 合法 YAML,但缺必填的 `name` —— 正是 `workflow ls` 空态提示曾经漏写的那个字段。
        fs::write(
            st.dir().join("broken.yaml"),
            "steps:\n  - id: a\n    agent: x\n    prompt: p\n",
        )
        .unwrap();
        assert_eq!(st.list_names(), vec!["broken".to_string()], "ls 应该列得出");
        let err = format!("{:#}", st.load("broken").expect_err("解析失败必须报错"));
        assert!(err.contains("解析失败"), "{err}");
        assert!(err.contains("name"), "报错要点出必填字段: {err}");
        assert!(!err.contains("无此工作流"), "不能误报成文件不存在: {err}");
    }
}
