# winshell

无边框窗口与多窗口弹窗的基础设施，供 Blitz 应用复用。与编辑器业务
无关，按依赖方向分为两层：

| 层 | 职责 | 依赖 |
| --- | --- | --- |
| `win` | 无边框窗口行为：拖动 / 缩放、双击标题栏、snap、DPI、owner 挂接、触摸键盘探测。**平台差异收在 `win/frameless/{win32,winit_impl}`**——公开门面（`win::frameless`）平台无关、无 cfg、无 HWND 参数；Windows 走 Win32，其余平台走 winit 原生手势 | Win32（仅 Windows）+ winit（经 `dioxus-native` 再导出） |
| `shell` | 主窗口启动、弹窗惰性创建与路由（拥属弹窗是 Win32 语义，见 `shell/win32`）、状态桥、弹窗控件库、动效。平台能力以运行时谓词暴露（`owned_popups()` / `native_ctx_menu()`） | `win` + Blitz 渲染栈 |

## 快速开始

```rust
use dioxus_native::prelude::*;
use winshell::{register_popups, Popup, PopupId};

fn main() {
    winshell::win::frameless::ensure_dpi_awareness();
    register_popups(&[&FIND_POPUP]);

    let attrs = dioxus_native::WindowAttributes::default()
        .with_title("My App")
        .with_decorations(false);
    let clear = dioxus_native::Color::from_rgba8(30, 30, 30, 255);
    winshell::shell::launch(app, attrs, clear);
}

#[component]
fn app() -> Element {
    rsx! { div { "hello" } }
}

struct FindPopup;
static FIND_POPUP: FindPopup = FindPopup;

impl Popup for FindPopup {
    fn id(&self) -> PopupId { PopupId("find") }
    fn title(&self) -> &'static str { "查找" }
    fn min_size(&self) -> (f32, f32) { (420.0, 180.0) }
    fn dark(&self) -> bool { true }
    fn root(&self) -> fn() -> Element { || rsx! { div { "查找面板" } } }
    fn flush(&self) {}
    fn on_focused(&self) {}
    fn request_close(&self) {}
}
```

应用侧还可以在 `launch` 之前注入三个钩子：

```rust
winshell::shell::set_net_provider(net_provider);  // 网络资源（图片等）
winshell::shell::set_log_path(|| Some(log_path)); // 诊断日志落点
winshell::shell::set_anchor_probe(probe);         // 主窗口布局锚点测量
```

## 模块

| 路径 | 内容 |
| --- | --- |
| `win::frameless` | 拖动 / 缩放热区、双击标题栏、min / max / close / topmost、DPI、owner 挂接 |
| `win::snap` | 子类化窗口过程，补齐 Win+↑ / 拖顶 snap |
| `win::softkb` | 触摸键盘（TabTip）探测 |
| `shell` | 自建多窗口 `ApplicationHandler`：主窗口上下文注入、弹窗创建与 `ShellEvt` 路由 |
| `shell::bridge` | `Bridge<S, C>` 双向状态桥、composer 焦点代数、光标闪烁记账 |
| `shell::controls` | 弹窗控件库（按钮 / 复选 / 单选 / 输入行 / 分组 / 列表 / 标题栏） |
| `shell::motion` | 动效令牌（hover / press 过渡） |
| `dialog` | 文件对话框 owner 与 DPI 感知的稳定入口 |

## 行为契约

以下几条是实机验证过的硬约束，改动前先读对应模块注释：

1. **owner 挂接**用 `SetWindowLongPtrW(GWLP_HWNDPARENT)`，不能换成 winit 的
   `with_owner_window`——后者会把窗口变成 `WS_POPUP`，丢掉系统标题栏与边框。
   每次显示前都要重新挂接：winit 的 `set_visible` 会重写扩展样式。
2. **拖动窗口**走 `PostMessageW(WM_NCLBUTTONDOWN)`，不用 winit 的 `drag_window`：
   后者的复位标志会被嵌套模态循环卡死，之后拖动与缩放静默失效。
3. 无边框窗口需 `with_undecorated_shadow(true)` 才有 DWM 阴影。
4. `WM_NCHITTEST` 命中测试中不能触碰 Dioxus `Signal`；交互组件矩形由布局层经
   `set_hit_exempt` 推送。
5. 弹窗光标闪烁必须事件驱动，不能用 CSS 动画——blitz 会对运行动画的窗口每帧
   续绘，挤占消息泵。
6. winit / raw-window-handle 类型一律经 `dioxus-native` 再导出使用，不要混用
   crates.io 上的 winit（类型不同源，无法传递）。

## 测试

```bash
cargo test -p winshell
```

仅支持 Windows。

## 开源协议

MIT，见仓库根目录的 [LICENSE](../../LICENSE)。
