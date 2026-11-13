//! 当前应用的 owner 只读通道，凭据留在 lark-cli。

use super::{bounded_output, LarkUser};
use crate::frontend::client::document_comments::CommentRead;
use crate::runtime::document_comments::CommentNotice;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

pub(crate) struct OwnerCommentReader {
    executable: String,
    app_id: String,
    owner_open_id: String,
    started: std::time::Instant,
}

impl OwnerCommentReader {
    pub(crate) fn new(executable: String, app_id: String, owner_open_id: String) -> Self {
        Self {
            executable,
            app_id,
            owner_open_id,
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn read(&self, notice: &CommentNotice, operation: CommentRead) -> Result<Value> {
        notice.validate()?;
        anyhow::ensure!(
            notice.app_id == self.app_id && notice.sender_open_id == self.owner_open_id,
            summon_i18n::format!("r741f9c4973")
        );
        let lark = self.verified_client()?.with_timeout(self.remaining()?);
        let data = lark.run(
            &read_args(notice, operation)?
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )?;
        anyhow::ensure!(
            serde_json::to_vec(&data)?.len() <= 2 * 1024 * 1024,
            summon_i18n::format!("r286eea1934")
        );
        let verified = self.verified_client()?;
        anyhow::ensure!(
            verified.profile == lark.profile,
            summon_i18n::format!("r1f30085bfc")
        );
        Ok(data)
    }
    fn remaining(&self) -> Result<Duration> {
        let left = Duration::from_secs(30)
            .checked_sub(self.started.elapsed())
            .filter(|duration| !duration.is_zero())
            .context(summon_i18n::text!("rb86d56f68c"))?;
        Ok(left.min(Duration::from_secs(5)))
    }

    fn verified_client(&self) -> Result<LarkUser> {
        let mut command = Command::new(&self.executable);
        command.args(["profile", "list"]);
        let output = bounded_output(&mut command, self.remaining()?)?;
        anyhow::ensure!(output.status.success(), summon_i18n::format!("r1f30085bfc"));
        let profiles: Value =
            serde_json::from_slice(&output.stdout).context(summon_i18n::text!("r1f30085bfc"))?;
        let profile = matching_profile(&profiles, &self.app_id)?;
        let lark = LarkUser::new(Some(profile), Some(self.executable.clone()))
            .with_timeout(self.remaining()?);
        let identity = lark.run(&[
            "api",
            "GET",
            "/open-apis/authen/v1/user_info",
            "--as",
            "user",
        ])?;
        anyhow::ensure!(
            !self.owner_open_id.is_empty()
                && identity["open_id"].as_str() == Some(self.owner_open_id.as_str()),
            summon_i18n::format!("r1f30085bfc")
        );
        Ok(lark)
    }
}

fn matching_profile(profiles: &Value, app: &str) -> Result<String> {
    let profiles = profiles
        .as_array()
        .context(summon_i18n::text!("r1f30085bfc"))?;
    let matches: Vec<_> = profiles
        .iter()
        .filter(|p| {
            p.get("appId")
                .or_else(|| p.get("app_id"))
                .and_then(Value::as_str)
                == Some(app)
        })
        .collect();
    anyhow::ensure!(
        !app.is_empty() && matches.len() == 1,
        summon_i18n::format!("r1f30085bfc")
    );
    matches[0]["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .context(summon_i18n::text!("r1f30085bfc"))
}

fn read_args(notice: &CommentNotice, operation: CommentRead) -> Result<Vec<String>> {
    // 端点与参数来自固定只读操作，调用方不能指定 URL、身份或任意请求体。
    let (method, url, body) = operation.request("https://open.feishu.cn", notice)?;
    let url = reqwest::Url::parse(&url)?;
    let params = Value::Object(
        url.query_pairs()
            .map(|(k, v)| (k.into_owned(), json!(v)))
            .collect(),
    );
    let mut args = vec![
        "api".into(),
        method.to_string(),
        url.path().into(),
        "--as".into(),
        "user".into(),
        "--params".into(),
        params.to_string(),
    ];
    if let Some(body) = body {
        args.extend(["--data".into(), body.to_string()]);
    }
    Ok(args)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    fn notice() -> CommentNotice {
        CommentNotice {
            app_id: "cli_test".into(),
            event_id: "evt".into(),
            file_token: "doc_test".into(),
            file_type: "docx".into(),
            comment_id: "100".into(),
            reply_id: "200".into(),
            sender_open_id: "ou_owner".into(),
            recipient_open_id: "ou_bot".into(),
        }
    }

    #[test]
    fn profiles_must_match_the_app_unambiguously() {
        assert_eq!(
            matching_profile(
                &json!([{"name":"other_name","appId":"cli_test"}]),
                "cli_test"
            )
            .unwrap(),
            "other_name"
        );
        for v in [
            json!([]),
            json!([{"name":"writer","appId":"cli_other"}]),
            json!([{"name":"a","appId":"cli_test"},{"name":"b","appId":"cli_test"}]),
            json!([{"name":"","appId":"cli_test"}]),
        ] {
            assert!(matching_profile(&v, "cli_test").is_err());
        }
    }

    #[test]
    fn user_commands_are_limited_to_current_document_reads() {
        let event = notice();
        for op in [
            CommentRead::Metadata,
            CommentRead::Comments,
            CommentRead::Replies("next&token".into()),
            CommentRead::Text,
        ] {
            let args = read_args(&event, op).unwrap();
            assert_eq!(&args[3..5], &["--as", "user"]);
            assert!(!args[2].contains('?'));
            if args[1] == "POST" {
                assert!(args[2].ends_with("/batch_query"));
            } else {
                assert_eq!(args[1], "GET");
            }
            if args[2].contains("/replies") {
                assert!(args[2].contains("doc_test/comments/100"));
                let params: Value = serde_json::from_str(&args[6]).unwrap();
                assert_eq!(params["page_token"], "next&token");
                assert_eq!(params["user_id_type"], "open_id");
            }
        }
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, OwnerCommentReader) {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("lark");
        crate::test_support::write_test_executable(
            &executable,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "${0}.calls"
if [ "$*" = 'profile list' ]; then
  printf '%s' '[{"name":"writer","appId":"cli_test"}]'
  exit 0
fi
[ "$1 $2 $3" = '--profile writer api' ] || exit 1
[ "$6 $7" = '--as user' ] || exit 1
case "$4 $5" in
 'GET /open-apis/authen/v1/user_info') data='{"open_id":"ou_owner"}' ;;
 'POST /open-apis/drive/v1/metas/batch_query') data='{"metas":[{"doc_token":"doc_test","doc_type":"docx","owner_id":"ou_other","title":"Title","url":"https://example.feishu.cn/docx/doc_test","latest_modify_time":"100"}]}' ;;
 'POST /open-apis/drive/v1/files/doc_test/comments/batch_query') data='{"items":[{"comment_id":"100","is_whole":false,"is_solved":false,"quote":"Quote"}]}' ;;
 'GET /open-apis/drive/v1/files/doc_test/comments/100/replies') data='{"items":[{"reply_id":"200","user_id":"ou_owner","content":{"elements":[{"type":"person","person":{"user_id":"ou_bot"}},{"type":"text_run","text_run":{"text":"Question"}}]}}],"has_more":false}' ;;
 'GET /open-apis/docx/v1/documents/doc_test/raw_content') data='{"content":"Document body"}' ;;
 *) exit 1 ;;
esac
printf '{"ok":true,"identity":"user","data":%s}' "$data"
"#,
        );
        let reader = OwnerCommentReader::new(
            executable.to_string_lossy().into_owned(),
            "cli_test".into(),
            "ou_owner".into(),
        );
        (dir, reader)
    }

    #[test]
    fn owner_reads_metadata_comments_and_text_through_a_pinned_user_profile() {
        let (dir, reader) = fixture();
        let executable = dir.path().join("lark");
        let fc = crate::frontend::client::FeishuClient::new(
            "feishu.cn",
            "app".into(),
            "secret".into(),
            true,
        );
        let context = fc
            .owner_mentioned_comment_context(&notice(), "ou_bot", "ou_owner", &|op| {
                reader.read(&notice(), op)
            })
            .unwrap();
        assert!(context.owner_mention);
        assert!(context.thread.contains("Question"));
        assert_eq!(
            reader.read(&notice(), CommentRead::Text).unwrap()["content"],
            "Document body"
        );
        let calls = std::fs::read_to_string(executable.with_extension("calls")).unwrap();
        assert_eq!(
            calls.lines().filter(|line| *line == "profile list").count(),
            8
        );
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.contains("user_info"))
                .count(),
            8
        );
        assert!(!calls.contains("api POST /open-apis/drive/v1/files/doc_test/comments/100/replies"));
    }

    #[test]
    fn user_or_profile_changes_during_a_read_discard_the_result() {
        for mode in ["user", "app"] {
            let dir = tempfile::tempdir().unwrap();
            let executable = dir.path().join("lark");
            std::fs::write(executable.with_extension("mode"), mode).unwrap();
            crate::test_support::write_test_executable(
                &executable,
                r#"#!/bin/sh
if [ "$*" = 'profile list' ]; then
  app=cli_test
  if [ -f "${0}.switched" ] && [ "$(cat "${0}.mode")" = app ]; then app=cli_other; fi
  printf '[{"name":"writer","appId":"%s"}]' "$app"
elif [ "$5" = '/open-apis/authen/v1/user_info' ]; then
  user=ou_owner
  if [ -f "${0}.switched" ]; then user=ou_other; fi
  printf '{"ok":true,"data":{"open_id":"%s"}}' "$user"
elif [ "$5" = '/open-apis/drive/v1/metas/batch_query' ]; then
  touch "${0}.switched"
  printf '%s' '{"ok":true,"data":{"metas":[]}}'
else
  exit 1
fi
"#,
            );
            let reader = OwnerCommentReader::new(
                executable.to_string_lossy().into_owned(),
                "cli_test".into(),
                "ou_owner".into(),
            );
            assert!(
                reader.read(&notice(), CommentRead::Metadata).is_err(),
                "{mode}"
            );
            assert!(executable.with_extension("switched").exists());
        }
    }

    #[test]
    fn changed_login_is_rejected_before_document_data_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("lark");
        crate::test_support::write_test_executable(
            &executable,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "${0}.calls"
if [ "$*" = 'profile list' ]; then
  printf '%s' '[{"name":"writer","appId":"cli_test"}]'
elif [ "$*" = '--profile writer api GET /open-apis/authen/v1/user_info --as user' ]; then
  printf '%s' '{"ok":true,"data":{"open_id":"ou_other"}}'
else
  exit 1
fi
"#,
        );
        let reader = OwnerCommentReader::new(
            executable.to_string_lossy().into_owned(),
            "cli_test".into(),
            "ou_owner".into(),
        );
        assert!(reader.read(&notice(), CommentRead::Metadata).is_err());
        let calls = std::fs::read_to_string(executable.with_extension("calls")).unwrap();
        assert_eq!(calls.lines().count(), 2);
        assert!(!calls.contains("metas"));
    }
}
