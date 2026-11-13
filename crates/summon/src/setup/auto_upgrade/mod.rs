//! 自动升级(daemon 一侧),与新版从哪个渠道来无关的那部分。
//!
//! daemon 起来后一条后台线程按 config.yaml 的 `upgrade:` 定期问发布源。有合适的新版 —— 更新、
//! 在跟随范围内、发布满 `min_age_hours`、本平台有包、没有退回过 —— 就把它取到本机:不碰 daemon
//! 与落点。然后等整机连续空闲 `idle_minutes`,关上准入闸([`crate::runtime::admission`])、在关着
//! 的状态下再确认一次空闲,才拉起守望进程([`watch`]):它用与 `summon upgrade` 同一段脚本让
//! daemon 换代,再观察新版,起不来或观察期内崩了就放回换代前的落点快照。守望进程之前先起兜底进程:
//! 本代退出后守望进程在就由它起新一代,它不在了、或过了上限仍没起来,就由兜底进程放回换代前的
//! 落点再起。
//!
//! 全程不往飞书发任何东西:过程进 `run/upgrade.log`,状态进 `run/auto-upgrade.json`,
//! `summon doctor` 摆出来。
//!
//! - [`channel`] 管发布渠道:新版从哪里查、怎么取到本机、换代前怎么复核;GitHub Releases 渠道
//!   在 [`github`];
//! - [`state`] 管状态文件、退回名单、启动对账;
//! - [`decide`] 管该不该采用一个新版、整机空不空;
//! - [`swap`] 管落点快照、兜底进程、关闸等停机、一次换代尝试与它的落账;
//! - [`runner`] 管后台线程与 doctor;
//! - [`watch`] 管守望进程。

mod channel;
mod decide;
mod github;
mod runner;
mod state;
mod swap;
pub(crate) mod watch;

pub(crate) use runner::{doctor_lines, spawn};
#[cfg(test)]
pub(crate) use state::Store;
pub(crate) use state::{lift_rollback, refuses_publish};
