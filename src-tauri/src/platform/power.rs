//! 阻止系统在长任务期间休眠。
//!
//! 归档压缩动辄跑一整夜，机器一睡任务就断（R15）。macOS 的正规做法是
//! `NSProcessInfo -beginActivityWithOptions:reason:`，比 `caffeinate` 子进程
//! 干净：进程退出时断言自动释放，不会留下把机器永远吊醒的僵尸。
//!
//! 只禁止**空闲休眠**，不碰显示器休眠，也不阻止用户主动合盖——
//! 合盖休眠是用户的明确意图，任何应用都不该拦。

/// 持有期间系统不会因空闲而休眠，drop 即释放。
///
/// 多个 guard 可以并存，macOS 会自己做引用计数。
pub struct PowerGuard {
    #[cfg(target_os = "macos")]
    token: Option<mac::Token>,
}

impl PowerGuard {
    /// `reason` 会显示在 `pmset -g assertions` 里，方便用户查是谁吊着机器。
    pub fn new(reason: &str) -> Self {
        #[cfg(target_os = "macos")]
        {
            Self { token: mac::begin(reason) }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = reason;
            Self {}
        }
    }

    /// 是否真的拿到了断言。拿不到不算错误——任务照跑，只是可能被休眠打断。
    pub fn is_active(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.token.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
}

impl Drop for PowerGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(token) = self.token.take() {
            mac::end(token);
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::Retained;
    use objc2::runtime::{NSObjectProtocol, ProtocolObject};
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

    /// `beginActivity` 返回的凭据，`endActivity` 时原样交回。
    ///
    /// SAFETY: 这里手动实现 `Send`。凭据是个不透明的 `NSObject`，没有线程亲和性
    /// （不像 `NSView` 那类必须在主线程碰的 UI 对象），其引用计数由 objc runtime
    /// 原子维护；`endActivity:` 也没有主线程要求。而任务调度器在工作线程上创建、
    /// 可能在另一个线程上 drop，所以必须跨线程移动。
    pub struct Token(Retained<ProtocolObject<dyn NSObjectProtocol>>);
    unsafe impl Send for Token {}

    pub fn begin(reason: &str) -> Option<Token> {
        let info = NSProcessInfo::processInfo();
        let reason = NSString::from_str(reason);
        // UserInitiated 已包含 IdleSystemSleepDisabled，但显式或上更表意，
        // 也防止将来上游改了组合常量的含义。
        let options =
            NSActivityOptions::UserInitiated | NSActivityOptions::IdleSystemSleepDisabled;
        Some(Token(info.beginActivityWithOptions_reason(options, &reason)))
    }

    pub fn end(token: Token) {
        let info = NSProcessInfo::processInfo();
        // SAFETY: token 只可能来自上面的 begin()，类型正确且未被释放过
        //（Token 无 Copy/Clone，move 进来即消费）。
        unsafe { info.endActivity(&token.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_round_trips() {
        // M0 的 FFI 最小用例：能拿到断言、能正常释放、不 panic。
        let g = PowerGuard::new("zigzag 单元测试");
        #[cfg(target_os = "macos")]
        assert!(g.is_active(), "macOS 上应当成功拿到 NSProcessInfo 活动断言");
        drop(g);
    }

    #[test]
    fn guards_can_nest() {
        let a = PowerGuard::new("任务 A");
        let b = PowerGuard::new("任务 B");
        drop(a); // 先放外层，B 仍应有效
        assert_eq!(b.is_active(), cfg!(target_os = "macos"));
        drop(b);
    }

    #[test]
    fn empty_reason_does_not_crash() {
        // NSString::from_str("") 是合法的，但值得钉一下，免得空字符串把 FFI 打崩。
        let _g = PowerGuard::new("");
    }
}
