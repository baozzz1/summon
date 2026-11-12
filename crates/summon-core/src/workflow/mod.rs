//! P4 工作流编排 **纯件**:DAG 解析 + 拓扑序(Kahn,带环检测)+ `ready_steps` 调度决策 +
//! `{step_id}` 占位注入。执行(每 step 复用 P1/P3 扇出起一个 run)在 summon `workflow_runner`。
//! 设计权威:`docs/superpowers/specs/2026-06-14-p4-workflow-and-mcp-design.md`。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// 工作流一步:id(被 `needs`/占位引用)+ 跑它的 agent(别名或 worker)+ prompt(可含 `{上游id}`)+
/// 依赖的上游 step id。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub agent: String,
    pub prompt: String,
    #[serde(default)]
    pub needs: Vec<String>,
}

/// 一份工作流声明(`~/.summon/workflows/<name>.yaml`)。steps 构成 DAG。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<Step>,
}

/// 工作流校验/调度错误(runner 据此回执)。
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowError {
    #[error("{}", summon_i18n::text!("ra318813421"))]
    Empty,
    #[error("{}", summon_i18n::format!("r156434dc59", _0 = .0))]
    DuplicateId(String),
    #[error("{}", summon_i18n::format!("r9bff2ae175", step = .step, dep = .dep))]
    UnknownDep { step: String, dep: String },
    #[error("{}", summon_i18n::text!("rf934f31486"))]
    Cycle,
}

impl Workflow {
    /// 校验 + 返回一个合法拓扑序(Kahn)。非空、id 唯一、`needs` 引用存在、无环。
    /// runner 起跑前调一次做 fail-closed 校验(环/悬挂依赖在跑之前就拦下)。
    pub fn topo_order(&self) -> Result<Vec<String>, WorkflowError> {
        if self.steps.is_empty() {
            return Err(WorkflowError::Empty);
        }
        let mut ids = HashSet::new();
        for s in &self.steps {
            if !ids.insert(s.id.as_str()) {
                return Err(WorkflowError::DuplicateId(s.id.clone()));
            }
        }
        for s in &self.steps {
            for d in &s.needs {
                if !ids.contains(d.as_str()) {
                    return Err(WorkflowError::UnknownDep {
                        step: s.id.clone(),
                        dep: d.clone(),
                    });
                }
            }
        }
        let mut indeg: HashMap<&str, usize> = self
            .steps
            .iter()
            .map(|s| (s.id.as_str(), s.needs.len()))
            .collect();
        // 入度 0 的入队;排序使输出稳定(同层按 id 字典序)。
        let mut queue: Vec<&str> = indeg
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&k, _)| k)
            .collect();
        queue.sort_unstable();
        let mut order = Vec::new();
        while let Some(id) = queue.pop() {
            order.push(id.to_string());
            for s in &self.steps {
                if s.needs.iter().any(|d| d == id) {
                    let e = indeg
                        .get_mut(s.id.as_str())
                        .expect(summon_i18n::text!("rf5f19b2d14"));
                    *e -= 1;
                    if *e == 0 {
                        queue.push(s.id.as_str());
                        queue.sort_unstable();
                    }
                }
            }
        }
        if order.len() != self.steps.len() {
            return Err(WorkflowError::Cycle);
        }
        Ok(order)
    }

    /// 给定已完成 step id 集,返回此刻可起的步骤(`needs` 全完成 + 自己未完成)。调度器每轮调,
    /// 这些步骤可并行起(并发上限沿用 P1)。
    pub fn ready_steps(&self, done: &HashSet<String>) -> Vec<&Step> {
        self.steps
            .iter()
            .filter(|s| !done.contains(&s.id) && s.needs.iter().all(|d| done.contains(d)))
            .collect()
    }
}

/// 把 prompt 里的 `{step_id}` 字面替换成该步产出(扩 P1 的 `{previous}` 到具名引用)。
/// 只替换 `results` 里有的;未提供的占位符原样留(正常调度下上游必先完成、必在 results 里)。
pub fn substitute_refs(prompt: &str, results: &HashMap<String, String>) -> String {
    let mut out = prompt.to_string();
    for (id, val) in results {
        out = out.replace(&format!("{{{id}}}"), val);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, needs: &[&str]) -> Step {
        Step {
            id: id.into(),
            agent: "w".into(),
            prompt: format!("do {id}"),
            needs: needs.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn topo_order_linear_and_diamond() {
        // 菱形:research → {a, b} → review。
        let wf = Workflow {
            name: "x".into(),
            steps: vec![
                step("research", &[]),
                step("a", &["research"]),
                step("b", &["research"]),
                step("review", &["a", "b"]),
            ],
        };
        let order = wf.topo_order().unwrap();
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("research") < pos("a") && pos("research") < pos("b"));
        assert!(pos("a") < pos("review") && pos("b") < pos("review"));
    }

    #[test]
    fn topo_order_catches_cycle_dup_unknown_empty() {
        // 环:a→b→a。
        let cyc = Workflow {
            name: "c".into(),
            steps: vec![step("a", &["b"]), step("b", &["a"])],
        };
        assert_eq!(cyc.topo_order(), Err(WorkflowError::Cycle));
        // 重复 id。
        let dup = Workflow {
            name: "d".into(),
            steps: vec![step("a", &[]), step("a", &[])],
        };
        assert!(matches!(
            dup.topo_order(),
            Err(WorkflowError::DuplicateId(_))
        ));
        // 悬挂依赖。
        let unk = Workflow {
            name: "u".into(),
            steps: vec![step("a", &["ghost"])],
        };
        assert!(matches!(
            unk.topo_order(),
            Err(WorkflowError::UnknownDep { .. })
        ));
        // 空。
        let empty = Workflow {
            name: "e".into(),
            steps: vec![],
        };
        assert_eq!(empty.topo_order(), Err(WorkflowError::Empty));
    }

    #[test]
    fn ready_steps_respects_deps() {
        let wf = Workflow {
            name: "x".into(),
            steps: vec![
                step("a", &[]),
                step("b", &["a"]),
                step("c", &["a"]),
                step("d", &["b", "c"]),
            ],
        };
        // 开局:只 a ready。
        let r = wf.ready_steps(&HashSet::new());
        assert_eq!(r.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["a"]);
        // a 完成 → b、c 并行 ready;d 还不行。
        let done: HashSet<String> = ["a".to_string()].into_iter().collect();
        let mut ids: Vec<&str> = wf
            .ready_steps(&done)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, ["b", "c"]);
        // a,b,c 完成 → d ready。
        let done: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            wf.ready_steps(&done)
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["d"]
        );
    }

    #[test]
    fn substitute_refs_replaces_named() {
        let mut r = HashMap::new();
        r.insert("research".to_string(), "结论X".to_string());
        assert_eq!(
            substitute_refs("基于 {research} 实现", &r),
            "基于 结论X 实现"
        );
    }
}
