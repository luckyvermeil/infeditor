# wysiwyg

**模型驱动**的 Markdown WYSIWYG 编辑器，以 Dioxus Native (Blitz) 组件形式提供。

`model::Doc`（块树）是唯一真相源，DOM 是纯派生：渲染后不再反向写回，Blitz
只承担只读渲染与排版查询。因此不需要 contenteditable，也不需要光标锚点 hack。

## 快速开始

```rust
use dioxus_native::prelude::*;
use wysiwyg::{parse_doc, Editor, Wysiwyg};

#[component]
fn App() -> Element {
    let editor = use_signal(|| Editor::new(parse_doc("# 标题\n\n正文")));

    rsx! {
        // 组件按父容器尺寸布局，容器需有明确高度
        div { style: "width:100%; height:100vh;",
            Wysiwyg { editor, font_size: Some(16) }
        }
    }
}
```

可运行的完整示例（编辑区 + 实时源码分屏）：

```bash
cargo run -p wysiwyg --example demo
```

## 模块

| 模块 | 职责 | GUI 依赖 |
| --- | --- | --- |
| `model` | 块树、`Pos` / `Sel` 逻辑坐标 | 无 |
| `md` | Markdown ↔ `Doc`（pulldown-cmark 解析 + 公式花括号遮蔽，自写 emit） | 无 |
| `edit` | 编辑命令（`Cmd` / `Move` / `TableCmd`）与 `Editor` 状态机 | 无 |
| `render` | `Doc` → HTML 字符串 | 无 |
| `preview` | 与编辑共用块树的语义 HTML 预览 | 无 |
| `math` | LaTeX → 自包含 SVG | 无 |
| `chem` | 预览专用的化学式 / 公式定界符归一 | 无 |
| `hl` | 代码块着色钩子（宿主按需注册） | 无 |
| `paste` | HTML 剪贴板 → 清洗 → Markdown | 无 |
| `dom_index` | `data-bid` → `NodeId` 的增量索引 | blitz-dom |
| `geom` | 点击 → `Pos`、`Pos` → 光标矩形的双向查询 | blitz-dom |
| `ui` | `Wysiwyg` 组件、事件接线、光标覆盖层 | dioxus-native |

前九个模块不含 GUI 依赖，可直接无头测试。

## 组件参数

| 参数 | 说明 |
| --- | --- |
| `editor: Signal<Editor>` | 必需；状态机由宿主持有 |
| `font_size: Option<i32>` | 正文字号，缺省 16 |
| `fg: Option<String>` | 内容前景色（暗色主题应传入，否则公式可能不可见） |
| `img_base: Option<PathBuf>` | 相对图片的解析基准，通常是文档所在目录 |
| `on_open_link` | Ctrl+点击链接的回调 |
| `on_edit` / `on_caret` | 鼠标编辑 / 光标移动后的通知，宿主据此同步外部状态 |
| `on_ready` | 组件挂载后交出 `NodeHandle` |
| `overlays` | 与宿主共用的光标 / 单元格覆盖层状态 |
| `grab_focus: Option<bool>` | 是否自行抢焦点；接入既有编辑器时应为 `false` |

## 接入宿主的约定

1. 宿主设 `grab_focus = false`，组件也不设 `tabindex`——否则按键会绕过宿主的
   输入桥，状态不同步。
2. 每次编辑后，宿主把内容写回自己的文档模型。infeditor 的实现是
   `src/ui/components/mdws.rs::sync_to_doc`：幂等、最小差异替换，撤销粒度与
   编辑器内一致。
3. `parley` 必须与 blitz 解析到同一个 rev，否则 `Layout<TextBrush>` 类型不互通。
4. blitz 的 inline 排版使用物理像素，读取 parley 几何时要除以缩放比。
5. 渲染期间不能访问 Blitz 文档（此时已被可变借用）；几何计算放在事件处理器里，
   结果存进 Signal 供渲染读取。

## 诊断

| 环境变量 | 作用 |
| --- | --- |
| `INF_WYS_LOG=1` | 输出光标几何与编辑诊断日志 |
| `INF_WYS_DUMP=<path>` | demo 专用：把 emit 后的 Markdown 转储到文件 |
| `INFEDITOR_IME=0` | 关闭 IME 通道（GUI 自动化用） |

## 测试

```bash
cargo test -p wysiwyg
```

其他示例：`cargo run -p wysiwyg --example math_probe`（公式渲染探针）、
`cargo run -p wysiwyg --example math_doc_probe -- <file.md>`（逐个公式体检一份文档）、
`cargo run -p wysiwyg --example md_normalize -- <file>`（样张规范化工具）。

## 开源协议

MIT，见仓库根目录的 [LICENSE](../../LICENSE)。
