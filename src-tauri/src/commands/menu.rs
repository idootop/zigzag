//! 原生菜单。
//!
//! 存在的理由只有一个：**快捷键得可靠**。
//!
//! 网页层的 `keydown` 要过三道关才轮得到——窗口得是 key window、webview 得是第一
//! 响应者、输入法还得肯把这个组合键放行。⌘, 恰好是最容易卡住的那个：同一台机器上
//! ⌘1/⌘2 次次都到，⌘, 却时灵时不灵（基准 39）。菜单的 key equivalent 不走这条路，
//! NSApp 在把事件派进响应者链**之前**先问菜单，输入法根本没机会插手。
//!
//! 所以三个快捷键整体搬到这儿，网页层不再自己监听 `keydown`——菜单吃掉的按键不会
//! 再传到 webview，留着那份监听只会让同一个键有两个可能失配的实现。
//!
//! 底座是 [`Menu::default`]（关于/服务/隐藏/退出、⌘Z/⌘X/⌘C/⌘V/⌘A、最小化/全屏、
//! 窗口、帮助），只往里**插**三项。不自己从头声明一份：那就得手工重建 Edit，
//! 稍有遗漏就静默丢掉命名模板输入框里的复制粘贴。
//!
//! **菜单栏保持英文，只有自己这三项是中文**，看着不齐，但试过了：muda 的预定义
//! 条目标题是硬编码英文（`muda/src/platform_impl/macos/mod.rs:361`），改得动；
//! AppKit 自己往 Window / Edit 里塞的那十几项（Minimize All、Move & Resize、
//! Start Dictation…）改不动。翻一半比不翻更难看，所以整块交给系统。
//! 自己这三项必须是中文——它们得和界面上写的「压缩」「查重」对得上，
//! 不然看了菜单也不知道它对应屏幕上的哪儿。

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    AppHandle, Emitter, Runtime,
};

/// 菜单项被触发时发给前端的事件，载荷是下面三个 id 之一。
///
/// 前端那边的常量在 `src/lib/ipc.ts`，改名必须两边一起改。
pub const EVENT_ACTION: &str = "menu://action";

pub const ID_SETTINGS: &str = "settings";
pub const ID_LANE_COMPRESS: &str = "lane-compress";
pub const ID_LANE_DEDUP: &str = "lane-dedup";

/// 在默认菜单上插入自己那三项。
pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let menu = Menu::default(app)?;
    let items = menu.items()?;

    // macOS 上第一项恒为应用同名子菜单，头两项是「关于」和分隔线（见
    // `Menu::default`），所以 2 就是「设置…」在系统里的固定位置。
    if let Some(app_menu) = items.first().and_then(|i| i.as_submenu()) {
        app_menu.insert_items(
            &[
                &MenuItem::with_id(app, ID_SETTINGS, "设置…", true, Some("CmdOrCtrl+,"))?,
                &PredefinedMenuItem::separator(app)?,
            ],
            2,
        )?;
    }

    // 两条线放 View 而不是新开一个子菜单：它们切的就是同一个窗口看什么，和它
    // 下面那个全屏是一类事。
    for item in &items {
        let Some(sub) = item.as_submenu() else { continue };
        if sub.text()? == "View" {
            sub.prepend_items(&[
                &MenuItem::with_id(app, ID_LANE_COMPRESS, "压缩", true, Some("CmdOrCtrl+1"))?,
                &MenuItem::with_id(app, ID_LANE_DEDUP, "查重", true, Some("CmdOrCtrl+2"))?,
                &PredefinedMenuItem::separator(app)?,
            ])?;
        }
    }

    Ok(menu)
}

/// 把菜单点击转成一条事件。状态全在前端，这一层只负责报信。
pub fn on_event<R: Runtime>(app: &AppHandle<R>, event: tauri::menu::MenuEvent) {
    let id = event.id().as_ref();
    // 只转发自己那三项。预定义项（退出、复制…）AppKit 已经自己处理完了。
    if matches!(id, ID_SETTINGS | ID_LANE_COMPRESS | ID_LANE_DEDUP) {
        let _ = app.emit(EVENT_ACTION, id);
    }
}
