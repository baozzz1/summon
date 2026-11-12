//! 控制台写入的本机审计:`~/.summon/run/dashboard-audit.ndjson`,每次写入追加一行。
//! 只记键路径,不记任何字段值(值里可能有密钥);日志页的「控制台审计」读的就是它。

use std::io::Write as _;
use std::path::Path;

use serde::Serialize;

use super::sessions::Principal;

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Identity<'a> {
    Owner,
    Feishu { open_id: &'a str, union_id: &'a str },
}

#[derive(Serialize)]
struct Line<'a> {
    ts: String,
    identity: Identity<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issuer_agent: Option<&'a str>,
    file: &'a str,
    keys: &'a [String],
    result: &'a str,
}

/// 追加一条记录。写不进去只记日志:审计失败不该让一次已经成功的保存报错。
pub(crate) fn record(
    path: &Path,
    principal: &Principal,
    file: &str,
    keys: &[String],
    result: &str,
) {
    let (identity, issuer_agent) = match principal {
        Principal::Owner => (Identity::Owner, None),
        Principal::Feishu(issuer) => (
            Identity::Feishu {
                open_id: &issuer.open_id,
                union_id: &issuer.union_id,
            },
            Some(issuer.agent.as_str()),
        ),
    };
    let line = Line {
        ts: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        identity,
        issuer_agent,
        file,
        keys,
        result,
    };
    let appended = (|| -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string(&line)?;
        text.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        // 单次 write 的追加在同一文件上不会与别的追加交错(行很短)。
        f.write_all(text.as_bytes())?;
        Ok(())
    })();
    if let Err(e) = appended {
        crate::observe::log::log(&summon_i18n::format!("r8c1f492989"));
    }
}
