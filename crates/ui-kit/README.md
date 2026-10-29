# ui-kit

面向 **Dioxus Native (Blitz)** 的通用 UI 组件库。组件以标准 HTML 元素承载
语义、用标准 CSS 伪类表达交互状态，不依赖任何应用业务概念，可直接复用到其他
Blitz 项目。

## 快速开始

```toml
[dependencies]
ui-kit = { path = "../ui-kit" }
```

```rust
use ui_kit::prelude::*;
use ui_kit::{Button, ButtonKind, Checkbox, KitStyle, DARK};

#[component]
fn App() -> Element {
    let theme = DARK;
    let mut agree = use_signal(|| false);

    rsx! {
        // 每个窗口根挂一次；主题切换时以 dark 为 key 重建 <style>
        KitStyle { theme, dark: theme.dark }

        Button {
            label: "确定",
            kind: ButtonKind::Primary,
            theme,
            on_click: move |_| { /* ... */ },
        }
        Checkbox {
            checked: agree(),
            label: "同意",
            theme,
            on_toggle: move |_| agree.set(!agree()),
        }
    }
}
```

`ui_kit::prelude` 再导出了 `dioxus_native::prelude`，宿主无需单独声明
`dioxus-native` 即可写 `rsx!`。

若要接入应用自己的主题，实现一次 `From` 转换即可（值拷贝，无分配）：

```rust
let theme: ui_kit::Theme = (&app_theme).into();
```

## 组件

| 组件 | 说明 |
| --- | --- |
| `Button` / `ButtonKind` | 主 / 次 / 轻 / 危险四档按钮，真 `<button>` |
| `IconButton` | 方形图标按钮 |
| `Checkbox` / `CheckMark` | 受控复选框；`CheckMark` 是纯视觉勾选件（菜单勾选列复用） |
| `Radio` | 受控单选，同名互斥由原生语义保证 |
| `TextInput` | 单行文本框的显示件，键盘输入由宿主路由 |
| `MenuPanel` / `MenuRowView` | 下拉菜单面板：几何、滚动、键盘光标与三态行 |
| `Modal` | 模态对话框外壳：遮罩 + 面板 + 标题 / 内容 / 动作行 |
| `Separator` | 分隔线，真 `<hr>` |

配套模块：

| 模块 | 内容 |
| --- | --- |
| `tokens` | 字体栈、字号刻度、圆角、控件尺寸 |
| `theme` | `Theme` 纯色板，内置 `LIGHT` / `DARK` |
| `style` | 按主题生成的类样式表与 `KitStyle` 注入组件 |
| `motion` | 动效令牌；自绘组件用的三态信号机 `Press` |
| `menu` 中的函数 | 面板高度、可见行数、滚动步长等几何计算 |

## 设计约定

1. **标准 HTML 优先**：按钮、复选框、分隔线都有原生元素与伪类，组件不自造
   状态机；只有菜单面板、模态外壳、显示型文本框等没有标准对应物的部件才自绘。
2. **受控组件**：状态由 props 传入，交互经 `EventHandler` 上抛，库内不持有业务状态。
3. **主题注入**：组件只认 `Theme` 色板，不引用应用主题类型。
4. **样式按主题生成**：颜色来自运行时主题，因此宿主必须在每个用到本库的窗口根
   挂一次 `KitStyle`；主题切换时靠 `key` 重建节点——blitz 只在 `<style>` 节点
   创建时解析样式文本，原地改文本不会重新解析。

## 容易踩的坑

- **布尔属性**：blitz 按属性存在性判断 `disabled`。自行封装时务必写
  `disabled: disabled.then_some(true)`；写成 `disabled: false` 会生成
  `disabled="false"`，按钮反而被禁用。
- **点击默认动作**：blitz 的 `click` 默认行为会清除焦点。库内组件已在
  `onpointerdown` / `onclick` 中 `prevent_default()`；宿主自定义的可点元素也要
  照做，否则会打断键盘焦点。
- **子窗口动画**：弹窗子窗口不要使用 CSS 动画（blitz 会每帧续绘，挤占消息泵）。
  `MODAL_CSS` 的入场动画只应注入主窗口。

## 测试

```bash
cargo test -p ui-kit
```

## 开源协议

MIT，见仓库根目录的 [LICENSE](../../LICENSE)。
