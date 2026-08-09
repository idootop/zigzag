/**
 * 应用外壳。
 *
 * 只剩三件事：启动时把库里的残局捞回来、把两条线之一挂上去、监听三个快捷键。
 * 工具栏**不在这儿**渲染——阶段主操作只有阶段自己知道，放上来就得靠 portal
 * 或 slot context 往上送，那是过度设计（见 `Toolbar.tsx`）。
 */
import { useEffect } from "react";

import { Toolbar } from "@/components/Toolbar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useApp } from "@/store/app";
import { useDedup } from "@/store/dedup";
import { useJob } from "@/store/job";
import { useUI } from "@/store/ui";
import { Compress } from "@/views/Compress";
import { Dedup } from "@/views/Dedup";
import { SettingsSheet } from "@/views/Settings";

export default function App() {
  const ready = useApp((s) => s.ready);
  const bootstrap = useApp((s) => s.bootstrap);
  const resumeDedup = useDedup((s) => s.resume);
  const checkResumable = useJob((s) => s.checkResumable);
  const lane = useUI((s) => s.lane);
  const settingsOpen = useUI((s) => s.settingsOpen);
  const setSettingsOpen = useUI((s) => s.setSettingsOpen);

  useEffect(() => {
    void bootstrap();
    // 上次查完还没处理的重复结果在库里等着。在这儿捞而不是等用户点进「查重」
    // 那一屏——不然分段上那个圆点永远不会亮，等于没提示。
    void resumeDedup();
    // 同理，上次没跑完的压缩任务。进度一直都在库里，崩溃残局启动时也已经收拾
    // 干净了，唯独没人在界面上把它捞出来——不问这一句，用户看到的就是一片空白的
    // 选目录屏，只能重扫一遍。这是 P3「可中断、可恢复」在界面上的落点。
    //
    // 注意**不按「哪条线有活儿」自动选 lane**：两个 resume 是并发的，谁先回来
    // 不确定，那会变成一个随机跳转。默认永远落在压缩，让分段上的徽标去说话。
    void checkResumable();
  }, [bootstrap, resumeDedup, checkResumable]);

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      // 只认纯 ⌘。⌥⌘1 之类是别人的组合键，别抢。
      if (!e.metaKey || e.ctrlKey || e.altKey) return;
      const ui = useUI.getState();
      if (e.key === ",") {
        e.preventDefault();
        ui.setSettingsOpen(!ui.settingsOpen);
        return;
      }
      // 面板开着时不换线：换了当场也看不见，等关掉面板才发现自己已经不在原来
      // 那条线上了。Esc 关面板是 Radix 自带的。
      if (ui.settingsOpen) return;
      if (e.key === "1") {
        e.preventDefault();
        ui.setLane("compress");
      } else if (e.key === "2") {
        e.preventDefault();
        ui.setLane("dedup");
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <TooltipProvider delayDuration={300}>
      <div className="flex h-full flex-col">
        {!ready ? (
          <>
            {/* 配置还没读回来也先把工具栏摆上：这半秒里窗口总得能拖。 */}
            <Toolbar />
            <div className="grid min-h-0 flex-1 place-items-center text-muted-foreground">
              载入中…
            </div>
          </>
        ) : lane === "compress" ? (
          <Compress />
        ) : (
          <Dedup />
        )}
      </div>

      {/* 两条线共用一个设置面板，所以它挂在外壳上而不是任何一条线里。 */}
      <SettingsSheet open={settingsOpen} onOpenChange={setSettingsOpen} />
    </TooltipProvider>
  );
}
