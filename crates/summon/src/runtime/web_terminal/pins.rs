//! 有网页连着的会话,窗口按此刻的尺寸钉住:`resize-window -x -y` 让 tmux 把窗口级 `window-size`
//! 设成 `manual`,并把这个尺寸记作 manual 尺寸,之后任何客户端都改不动它。
//!
//! 不用 `attach -f ignore-size`:它只在同一 tmux 服务上还有别的普通客户端时才生效,网页是唯一
//! 客户端时照样按浏览器尺寸改窗口,断开后也不改回来。也不单设 `window-size manual`:它把窗口弹回
//! 创建时的尺寸,被本地终端撑大过的 agent 窗口会当场缩回去。
//!
//! 最后一条网页连接断开时恢复窗口级 `window-size` 的原取值;窗口尺寸停在钉住时的样子,下一个
//! 本地客户端接入时按原取值照常重算。daemon 优雅停机时按钉住表一次恢复全部(`release_all`)。
//!
//! 记账的单位是**窗口 id**(见 [`WindowId`]),不是会话:`window-size` 是窗口级选项,而
//! `={会话}:` 指的是此刻的当前窗口。同一会话的两个窗口各钉各的,恢复只落回钉住的那一个。

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

use crate::session::tmux::{Tmux, WindowId};

pub(super) trait WindowSizeOps {
    /// 会话此刻的当前窗口。钉住在这一刻定下窗口,之后的取值、写回都只认它。
    fn current_window(&self, session: &str) -> io::Result<WindowId>;
    /// 窗口级 `window-size` 的现有取值;None = 窗口上没单独设,沿用全局。
    fn get(&self, window: &WindowId) -> io::Result<Option<String>>;
    /// 按窗口此刻的尺寸钉住。
    fn pin(&self, window: &WindowId) -> io::Result<()>;
    /// 写回窗口级取值;None = 撤掉窗口级设置。
    fn restore(&self, window: &WindowId, value: Option<&str>) -> io::Result<()>;
}

pub(super) struct TmuxWindow;

impl WindowSizeOps for TmuxWindow {
    fn current_window(&self, session: &str) -> io::Result<WindowId> {
        Tmux::new(session.to_string()).current_window()
    }
    fn get(&self, window: &WindowId) -> io::Result<Option<String>> {
        window.size_option()
    }
    fn pin(&self, window: &WindowId) -> io::Result<()> {
        let geometry = window.geometry()?;
        window.resize(geometry.cols, geometry.rows)
    }
    fn restore(&self, window: &WindowId, value: Option<&str>) -> io::Result<()> {
        window.set_size_option(value)
    }
}

#[derive(Default)]
pub(super) struct WindowPins {
    inner: Mutex<PinTable>,
}

#[derive(Default)]
struct PinTable {
    /// 窗口 id → (在用的网页连接数, 钉住前的窗口级取值)。按窗口 id 而不是会话名记账:
    /// `window-size` 是窗口级选项,同一会话的两个窗口各钉各的,恢复也只落回钉住的那个窗口。
    /// 读取与写回都在锁内做,否则并发的第一次钉住会把另一条连接刚钉出来的 `manual` 当成原取值
    /// 存下来,恢复时就恢复错了。
    windows: HashMap<WindowId, (usize, Option<String>)>,
    /// 停机时已全部恢复:之后再钉就没人恢复了,一律拒绝。
    released: bool,
}

pub(super) struct PinGuard<'a, O: WindowSizeOps> {
    pins: &'a WindowPins,
    ops: &'a O,
    window: WindowId,
}

impl<O: WindowSizeOps> PinGuard<'_, O> {
    /// 这条连接钉住的那个窗口。开 pty 要按它读尺寸:会话的当前窗口随时会换。
    pub(super) fn window(&self) -> &WindowId {
        &self.window
    }
}

impl WindowPins {
    pub(super) fn acquire<'a, O: WindowSizeOps>(
        &'a self,
        ops: &'a O,
        session: &str,
    ) -> io::Result<PinGuard<'a, O>> {
        let mut table = self
            .inner
            .lock()
            .map_err(|_| io::Error::other(summon_i18n::text!("rbda5463e1b")))?;
        if table.released {
            return Err(io::Error::other(summon_i18n::text!("ra80abbe201")));
        }
        // 窗口在锁内定下来:同一会话的两条连接在这里各自解析出的窗口可能不是同一个,表按窗口记账。
        let window = ops.current_window(session)?;
        if let Some(entry) = table.windows.get_mut(&window) {
            entry.0 += 1;
        } else {
            let original = ops.get(&window)?;
            ops.pin(&window)?;
            table.windows.insert(window.clone(), (1, original));
        }
        Ok(PinGuard {
            pins: self,
            ops,
            window,
        })
    }

    /// 把钉住表里的每个窗口恢复原取值并清空表,返回恢复了几个。之后还在的连接断开时找不到
    /// 表项,不重复恢复;之后的钉住一律拒绝。
    pub(super) fn release_all<O: WindowSizeOps>(&self, ops: &O) -> usize {
        let Ok(mut table) = self.inner.lock() else {
            return 0;
        };
        table.released = true;
        let pinned: Vec<_> = table.windows.drain().collect();
        for (window, (_, original)) in &pinned {
            // 窗口已经没了时恢复会失败:没有窗口可恢复,忽略即可。
            let _ = ops.restore(window, original.as_deref());
        }
        pinned.len()
    }
}

impl<O: WindowSizeOps> Drop for PinGuard<'_, O> {
    fn drop(&mut self) {
        let Ok(mut table) = self.pins.inner.lock() else {
            return;
        };
        let Some(entry) = table.windows.get_mut(&self.window) else {
            return;
        };
        entry.0 -= 1;
        if entry.0 == 0 {
            let original = entry.1.clone();
            table.windows.remove(&self.window);
            // 窗口已经没了时恢复会失败:没有窗口可恢复,忽略即可。
            let _ = self.ops.restore(&self.window, original.as_deref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn win(id: &str) -> WindowId {
        WindowId::parse(id).unwrap()
    }

    /// 假 tmux 服务:每个会话有一个「当前窗口」,每个窗口各有一个窗口级 `window-size` 取值。
    /// `pin` 像 tmux 的 `resize-window` 一样把取值变成 `manual`。
    #[derive(Default)]
    struct FakeServer {
        current: RefCell<HashMap<String, WindowId>>,
        values: RefCell<HashMap<WindowId, String>>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeServer {
        /// 会话切到另一个窗口(窗格里的程序自己 `new-window`,或本地客户端按 `C-b c`)。
        fn switch(&self, session: &str, window: &str) {
            self.current
                .borrow_mut()
                .insert(session.to_string(), win(window));
        }
        fn value(&self, window: &str) -> Option<String> {
            self.values.borrow().get(&win(window)).cloned()
        }
        fn set(&self, window: &WindowId, value: Option<&str>) {
            let mut values = self.values.borrow_mut();
            match value {
                Some(v) => values.insert(window.clone(), v.to_string()),
                None => values.remove(window),
            };
        }
    }

    impl WindowSizeOps for FakeServer {
        fn current_window(&self, session: &str) -> io::Result<WindowId> {
            self.current
                .borrow()
                .get(session)
                .cloned()
                .ok_or_else(|| io::Error::other("会话不在"))
        }
        fn get(&self, window: &WindowId) -> io::Result<Option<String>> {
            Ok(self.values.borrow().get(window).cloned())
        }
        fn pin(&self, window: &WindowId) -> io::Result<()> {
            self.set(window, Some("manual"));
            self.calls.borrow_mut().push(format!("pin {window}"));
            Ok(())
        }
        fn restore(&self, window: &WindowId, value: Option<&str>) -> io::Result<()> {
            self.set(window, value);
            self.calls
                .borrow_mut()
                .push(format!("restore {window} {value:?}"));
            Ok(())
        }
    }

    /// 起一台假服务,`sessions` 是 (会话名, 当前窗口 id)。
    fn server(sessions: &[(&str, &str)]) -> FakeServer {
        let s = FakeServer::default();
        for (session, window) in sessions {
            s.switch(session, window);
        }
        s
    }

    #[test]
    fn the_window_is_pinned_while_any_page_is_open_and_restored_after_the_last() {
        let pins = WindowPins::default();
        let w = server(&[("s", "@1")]);
        let a = pins.acquire(&w, "s").unwrap();
        assert_eq!(w.value("@1").as_deref(), Some("manual"));
        let b = pins.acquire(&w, "s").unwrap();
        drop(a);
        assert_eq!(w.value("@1").as_deref(), Some("manual"), "还有页面开着");
        drop(b);
        assert_eq!(w.value("@1"), None, "窗口上原本没设,恢复成没设");
        assert_eq!(
            *w.calls.borrow(),
            vec!["pin @1".to_string(), "restore @1 None".to_string()],
            "同一窗口只钉一次、只恢复一次"
        );
    }

    #[test]
    fn an_explicit_original_value_is_restored_verbatim() {
        let pins = WindowPins::default();
        let w = server(&[("s", "@1")]);
        w.set(&win("@1"), Some("largest"));
        drop(pins.acquire(&w, "s").unwrap());
        assert_eq!(w.value("@1").as_deref(), Some("largest"));
    }

    /// 钉住之后会话切到了别的窗口:恢复必须落回钉住的那个窗口。按会话寻址(`={会话}:` = 此刻的
    /// 当前窗口)的话,这一步会把 `manual` 写到新窗口上、原窗口永远停在 `manual`,而且写是成功的,
    /// 没有任何报错。
    #[test]
    fn the_pin_is_read_and_restored_on_the_window_it_was_taken_on() {
        let pins = WindowPins::default();
        let w = server(&[("s", "@1")]);
        w.set(&win("@2"), Some("largest"));
        let pin = pins.acquire(&w, "s").unwrap();
        assert_eq!(pin.window(), &win("@1"));
        w.switch("s", "@2");
        drop(pin);
        assert_eq!(
            *w.calls.borrow(),
            vec!["pin @1".to_string(), "restore @1 None".to_string()],
            "钉住与恢复都认钉住那一刻的窗口"
        );
        assert_eq!(w.value("@1"), None, "原窗口恢复成没设");
        assert_eq!(
            w.value("@2").as_deref(),
            Some("largest"),
            "后来的当前窗口从没被碰过"
        );
    }

    /// 停机时连接线程不被 join:按钉住表一次恢复全部;之后连接断开不重复恢复,新的钉住被拒绝。
    #[test]
    fn a_shutdown_restores_every_pinned_window_once() {
        let pins = WindowPins::default();
        let w = server(&[("a", "@1"), ("b", "@2"), ("c", "@3")]);
        w.set(&win("@1"), Some("largest"));
        let a = pins.acquire(&w, "a").unwrap();
        let a2 = pins.acquire(&w, "a").unwrap();
        let b = pins.acquire(&w, "b").unwrap();
        assert_eq!(pins.release_all(&w), 2);
        assert_eq!(w.value("@1").as_deref(), Some("largest"));
        assert_eq!(w.value("@2"), None, "原本没设的撤掉窗口级设置");
        let calls = w.calls.borrow().len();
        assert_eq!(calls, 4, "两次钉住、两次恢复:{:?}", w.calls.borrow());
        drop((a, a2, b));
        assert_eq!(w.calls.borrow().len(), calls, "连接后断开不重复恢复");
        assert!(pins.acquire(&w, "c").is_err(), "停机后拒绝新的钉住");
        assert_eq!(w.value("@3"), None);
        assert_eq!(pins.release_all(&w), 0);
    }

    /// 每个窗口各钉各的,同一会话的两个窗口也一样。
    #[test]
    fn each_window_is_pinned_and_restored_on_its_own() {
        let pins = WindowPins::default();
        let w = server(&[("a", "@1"), ("b", "@2")]);
        let a = pins.acquire(&w, "a").unwrap();
        let b = pins.acquire(&w, "b").unwrap();
        drop(a);
        assert_eq!(
            *w.calls.borrow(),
            vec![
                "pin @1".to_string(),
                "pin @2".to_string(),
                "restore @1 None".to_string()
            ]
        );
        drop(b);

        // 同一个会话的第二个窗口:另起一份钉住,互不影响。
        let w = server(&[("s", "@1")]);
        let first = pins.acquire(&w, "s").unwrap();
        w.switch("s", "@2");
        let second = pins.acquire(&w, "s").unwrap();
        assert_eq!(w.value("@1").as_deref(), Some("manual"));
        assert_eq!(w.value("@2").as_deref(), Some("manual"));
        drop(second);
        assert_eq!(w.value("@1").as_deref(), Some("manual"), "第一个还开着");
        assert_eq!(w.value("@2"), None);
        drop(first);
        assert_eq!(w.value("@1"), None);
    }

    /// 读不到会话的当前窗口就不钉:钉不住窗口时连接不该接下去。
    #[test]
    fn a_session_without_a_current_window_is_never_pinned() {
        let pins = WindowPins::default();
        let w = server(&[]);
        assert!(pins.acquire(&w, "s").is_err());
        assert!(w.calls.borrow().is_empty());
    }

    /// 真 tmux:钉住之后会话的当前窗口换了(窗格里的程序自己开了一个新窗口并切过去),恢复仍落回
    /// 钉住的那个窗口,新窗口一个字节都没被改过。
    #[test]
    #[ignore]
    fn a_real_pin_is_restored_on_its_own_window_after_the_session_switches_windows() {
        let session = crate::test_support::ThrowawaySession::start("summon-wt-pinwin");
        let tmux = &session.0;
        let ops = TmuxWindow;
        let pins = WindowPins::default();
        let first = tmux.current_window().unwrap();
        assert_eq!(first.size_option().unwrap(), None, "前提:窗口级没设");
        let pin = pins.acquire(&ops, &tmux.session).unwrap();
        assert_eq!(pin.window(), &first);
        assert_eq!(first.size_option().unwrap().as_deref(), Some("manual"));

        // 窗格里的程序自己开一个窗口就是这个形状:新窗口成为会话的当前窗口。
        let out = std::process::Command::new("tmux")
            .args([
                "new-window",
                "-t",
                &format!("={}:", tmux.session),
                "-P",
                "-F",
                "#{window_id}",
                "sh",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let second = WindowId::parse(&String::from_utf8_lossy(&out.stdout)).unwrap();
        assert_ne!(second, first);
        assert_eq!(tmux.current_window().unwrap(), second, "当前窗口已经换了");

        drop(pin);
        let restored = first.size_option().unwrap();
        let untouched = second.size_option().unwrap();
        // 第二个窗口里的 shell 自己退出(不用销毁命令);会话由 ThrowawaySession 收尾。
        let _ = std::process::Command::new("tmux")
            .args(["send-keys", "-t", second.as_target(), "exit", "Enter"])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(restored, None, "恢复落回钉住的那个窗口");
        assert_eq!(untouched, None, "第二个窗口从没被碰过");
    }
}
