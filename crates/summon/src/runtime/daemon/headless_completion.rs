use crate::capture::{RerunRequest, TurnOutcome};
use crate::frontend::StreamId;

/// 停止等待只证明是否收到 worker 终态；通道关闭或期限到达都可能没有结果。
pub(super) enum WorkerStop {
    NoHandle,
    Reported(anyhow::Result<TurnOutcome>),
    Unconfirmed,
}

impl WorkerStop {
    pub(super) fn from_wait(result: Option<anyhow::Result<TurnOutcome>>) -> Self {
        match result {
            Some(result) => Self::Reported(result),
            None => Self::Unconfirmed,
        }
    }

    fn into_result(self, timeout: &'static str) -> anyhow::Result<TurnOutcome> {
        match self {
            Self::NoHandle => Ok(TurnOutcome::default()),
            Self::Reported(result) => result,
            Self::Unconfirmed => Err(anyhow::anyhow!(timeout)),
        }
    }
}

/// 执行收尾与消息是否送达相互独立；只有 worker 终点可决定是否安全 join。
pub(super) enum HeadlessExit {
    Completed(anyhow::Result<TurnOutcome>),
    Disconnected,
    TimedOut(WorkerStop),
    SteerUnconfirmed(WorkerStop),
    Interrupted(WorkerStop),
    Rerun {
        stopped: WorkerStop,
        request: RerunRequest,
        live_id: Option<StreamId>,
        timeout: &'static str,
    },
}

pub(super) enum Continuation {
    Complete,
    Interrupted,
    Rerun {
        request: RerunRequest,
        live_id: Option<StreamId>,
    },
}

impl Continuation {
    pub(super) fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted)
    }

    pub(super) fn is_rerun(&self) -> bool {
        matches!(self, Self::Rerun { .. })
    }

    pub(super) fn into_rerun(self) -> (Option<RerunRequest>, Option<StreamId>) {
        match self {
            Self::Rerun { request, live_id } => (Some(request), live_id),
            Self::Complete | Self::Interrupted => (None, None),
        }
    }
}

impl HeadlessExit {
    pub(super) fn observed_worker_end(&self) -> bool {
        match self {
            Self::Completed(_) | Self::Disconnected => true,
            Self::TimedOut(stop)
            | Self::SteerUnconfirmed(stop)
            | Self::Interrupted(stop)
            | Self::Rerun { stopped: stop, .. } => matches!(stop, WorkerStop::Reported(_)),
        }
    }

    pub(super) fn into_outcome(self) -> (anyhow::Result<TurnOutcome>, Continuation) {
        match self {
            Self::Completed(result) => (result, Continuation::Complete),
            Self::Disconnected => (Ok(TurnOutcome::default()), Continuation::Complete),
            Self::TimedOut(stop) => {
                let outcome = match stop.into_result(summon_i18n::text!("r35099d9f5e")) {
                    Ok(mut outcome) => {
                        outcome.timed_out = true;
                        outcome
                    }
                    Err(error) => TurnOutcome {
                        timed_out: true,
                        api_error: Some(error.to_string()),
                        ..Default::default()
                    },
                };
                (Ok(outcome), Continuation::Complete)
            }
            Self::SteerUnconfirmed(_) => (
                Err(anyhow::anyhow!(summon_i18n::format!("rf94ce96a80"))),
                Continuation::Complete,
            ),
            Self::Interrupted(stop) => (
                stop.into_result(summon_i18n::text!("r2781b7c6ab")),
                Continuation::Interrupted,
            ),
            Self::Rerun {
                stopped,
                request,
                live_id,
                timeout,
            } => (
                stopped.into_result(timeout),
                Continuation::Rerun { request, live_id },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reported() -> WorkerStop {
        WorkerStop::Reported(Ok(TurnOutcome {
            reply: Some("正文".into()),
            ..Default::default()
        }))
    }

    #[test]
    fn watchdog_keeps_partial_and_marks_timeout_for_each_worker_end() {
        let exit = HeadlessExit::TimedOut(reported());
        assert!(exit.observed_worker_end());
        let (result, continuation) = exit.into_outcome();
        let result = result.unwrap();
        assert!(result.timed_out);
        assert_eq!(result.reply.as_deref(), Some("正文"));
        assert!(!continuation.is_rerun());
        assert!(!continuation.is_interrupted());

        for stop in [
            WorkerStop::Reported(Err(anyhow::anyhow!("provider 退出失败"))),
            WorkerStop::Unconfirmed,
        ] {
            let (result, _) = HeadlessExit::TimedOut(stop).into_outcome();
            let result = result.unwrap();
            assert!(result.timed_out);
            assert!(result.api_error.is_some());
        }
    }

    #[test]
    fn unconfirmed_steer_is_failed_without_rerun_even_after_worker_success() {
        for stopped in [reported(), WorkerStop::Unconfirmed] {
            let exit = HeadlessExit::SteerUnconfirmed(stopped);
            let (result, continuation) = exit.into_outcome();
            assert!(result.unwrap_err().to_string().contains("可能已被处理"));
            assert!(!continuation.is_interrupted());
            assert!(!continuation.is_rerun());
        }
    }

    #[test]
    fn interrupt_and_rerun_keep_distinct_continuation_ownership() {
        let (result, continuation) = HeadlessExit::Interrupted(reported()).into_outcome();
        assert!(result.is_ok());
        assert!(continuation.is_interrupted());
        assert!(!continuation.is_rerun());
        let request = RerunRequest {
            prompt: "追加".into(),
            message_ids: vec!["message-2".into()],
            cause: crate::capture::RerunCause::NativeSteerFallback,
        };
        let (result, continuation) = HeadlessExit::Rerun {
            stopped: WorkerStop::Unconfirmed,
            request,
            live_id: None,
            timeout: "重跑收尾超时",
        }
        .into_outcome();
        assert_eq!(result.unwrap_err().to_string(), "重跑收尾超时");
        assert!(!continuation.is_interrupted());
        assert!(continuation.is_rerun());
        let (request, live_id) = continuation.into_rerun();
        assert_eq!(request.unwrap().message_ids, ["message-2"]);
        assert!(live_id.is_none());
    }

    #[test]
    fn safe_join_requires_an_observed_worker_end() {
        assert!(HeadlessExit::Completed(Ok(TurnOutcome::default())).observed_worker_end());
        assert!(HeadlessExit::Disconnected.observed_worker_end());
        assert!(HeadlessExit::Interrupted(reported()).observed_worker_end());
        assert!(!HeadlessExit::TimedOut(WorkerStop::Unconfirmed).observed_worker_end());
        assert!(!HeadlessExit::Interrupted(WorkerStop::NoHandle).observed_worker_end());
        assert!(matches!(
            WorkerStop::from_wait(None),
            WorkerStop::Unconfirmed
        ));
    }
}
