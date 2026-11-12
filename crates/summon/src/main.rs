mod bridge;
mod capture;
mod cli;
mod codex;
mod frontend;
mod memory;
mod observe;
mod remote;
mod remote_tools;
mod runtime;
mod schedule;
mod session;
mod setup;
#[cfg(test)]
mod test_support;
mod version;
mod workflow;

fn main() -> anyhow::Result<()> {
    // 第一件事,早于解析/线程/spawn(`set_var` 只在单线程期安全)。被 launchd 起的 Menu Bar
    // spawn 我们时 PATH 里没有 tmux —— 不补的话 `agents status` 会把所有 claude 会话
    // 当成不存在,且不报任何错(见 setup::env)。
    setup::env::heal_tool_path();
    setup::env::sanitize_tool_path();
    let cli = cli::parse();
    cli::run(cli)
}
