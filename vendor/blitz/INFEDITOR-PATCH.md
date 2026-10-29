# InfEditor 对 blitz 的本地补丁

本目录是 `https://github.com/dioxuslabs/blitz` 的 vendored 副本（与
Cargo.lock 锁定的 git commit 一致），经 `Cargo.toml` 的
`[patch."https://github.com/dioxuslabs/blitz"]` 接入构建。

## 补丁一：packages/blitz-dom/src/accessibility.rs

**问题**：blitz 建 accesskit 树时，文本节点只调 `set_value(text)` +
`parent.push_labelled_by(id)`，从不设置 `label`。而 accesskit_consumer 解析
`labelled_by` 关系时读的是被引节点的 **`label` 字段**（`write_label_direct`），
且 `descendant_label_filter` 回退只认 `Role::Label`/`Role::Image`——结果
UIA / 屏幕阅读器拿到的**所有元素名称都是空的**，只剩角色骨架。

**修法**（HTML-AAM §4.3 name from content）：

1. 交互元素（button / menuitem / tab / link / checkbox / radio）在建树时
   从子树文本计算 accessible name，写入 `set_label`；
2. 收集文本时跳过 `aria-hidden="true"` 子树与嵌套的交互控件（标签页里的
   关闭按钮不应混进标签名）；
3. 文本节点（TextRun）同时 `set_value` + `set_label`。

**验证**：`tests_py/test_a11y.py`（UIA 全树名称断言 + 纯键盘菜单操作）。

## 补丁二：packages/blitz-dom/src/node/node.rs（hit_inner 尊重 overflow 裁剪）

**问题**：`hit_inner` 判定后代是否可命中时用 `matches_content` /
`matches_overflow`（基于 scrollable overflow rect），**完全不考虑自身的
`overflow` 裁剪**。一个 `overflow:hidden` 容器里用负 top/translate 平移
出去的内容盒，其 scrollable overflow 仍会把容器向外撑大——视觉上已被裁掉
的区域照样能被命中，吞掉下层 UI 的点击。

在 InfEditor 里的表现：Markdown 预览（`mdpreview.rs`）根元素
`overflow:hidden`，内容盒 `top = 16 - scroll` 往下滚后向上伸出根元素，
滚过一屏后预览的隐形部分盖住工具栏，点击工具栏按钮无响应（"有时" =
滚动后）。

**修法**：`hit_inner` 在算完 `matches_hoisted_content` 之后加一段：若节点
自身 `overflow_x/y != Visible`（裁剪容器），把命中点换回**盒坐标**
（减去 `scroll_offset`——blitz 的 x/y 此时处于未滚动内容坐标系，滚动容器
内可视点的未滚动坐标可以超过 size，直接用 x/y 会误杀正常滚动容器），
点在自身 border box 外则提前 `return None`，阻止被视觉裁剪的后代参与
命中。hoisted（stacking context）内容豁免——fixed 定位后代本就不受祖先
裁剪。

**应用层配套**（不属于本补丁）：`mdpreview.rs` 根元素去掉
`user-select:none`、`editor.rs` 的 GLOBAL_CSS 把 `.md-body` 规则从
`user-select:none` 改为整段删除（见补丁四说明），恢复预览文本选中。

## 补丁三：packages/blitz-paint/src/lib.rs（选择高亮半透明）

**问题**：DOM 文本选择高亮 `SELECTION_COLOR` 是硬编码不透明
`#b4d5ff`（180,213,255）。InfEditor 的暗色主题下预览文字是浅色，不透明
亮蓝底一刷，选中的字直接被洗白看不清；且颜色与主题脱节。

**修法**：改 `Color::from_rgba8(100, 140, 255, 110)`（≈43% 透明度蓝），
半透明高亮在明/暗两种主题下都保持底字可读。此路径（`get_text_selection_ranges`
→ `draw_text_selection`）在 InfEditor 里只作用于 Markdown 预览的 DOM 选择
——编辑器正文选择是 viewport.rs 自绘的，输入框选择走 input editor 的
另一条路径，均不受影响。

## 补丁四：packages/blitz-dom/src/document.rs（段落间空隙起拖可选）

**问题**：`find_text_position` 要求命中节点有 `inline_root_ancestor`。
段落的 margin 空隙（行距 14px / 38px，约 37% 的垂直空间）不属于任何
段落盒，mousedown 落在那里命中**块容器**（无 inline 布局）→ 返回
None → mousedown `clear_text_selection`，选择锚点建不起来——从段落间
空隙或行间空白起拖永远选不中任何文本。浏览器会把这种拖动映射到最近
的文本位置，blitz 没有 fallback。

**修法**：主路径（命中节点自带 inline 布局）不变；失败时走新增的
`nearest_text_descendant`——从容器出发沿 layout children 递归，选
border box 距点最近的分支，直到找到自带 inline 布局的节点，把点换算进
其局部坐标（clamp 进盒内）后用 `text_offset_at_point` 解析字节偏移。

**注意**：显式 `user-select: text` 会**破坏**拖选——blitz 拖选门控
（pointer.rs `handle_pointermove`）只在 mousedown 节点计算值为
`UserSelect::Auto` 时进入 Selecting（Auto 再查直接父级；`Text`/`All`
两个分支都不处理）。应用层因此必须让预览叶子保持默认 Auto，不能钉
text（见 `src/ui/editor.rs` GLOBAL_CSS 注释）。

## 补丁五：packages/blitz-dom/src/events/pointer.rs（click 回退不清输入框焦点）

**问题**：`handle_click`（click 的默认动作）从命中节点向上找交互元素
（checkbox/radio/file/label/text-input）；全都没命中（即点击落在普通
div 上）就无条件 `doc.clear_focus()`。InfEditor 的整个 UI 都是普通 div
包着唯一一个透明 composer textarea（键盘/IME 的唯一着落点），于是点击
任意空白处都会把 composer 焦点抹掉、键盘失灵。

这个 bug 原本被应用层用「根元素 `onclick` 全局 `prevent_default()`」压住
了——但这等于把 click 的**全部**默认动作都关掉，而 dblclick 的合成恰恰
住在 handle_click 里：CSV 表格视图的双击编辑（`ondoubleclick`）因此从未
生效过（真实鼠标同样中招，测试由 `tests_py/test_csvtable.py` 抓出）。

**修法**：clear_focus 回退前检查当前焦点节点——是文本输入框
（`SpecialElementData::TextInput`）就跳过清除。语义上更接近浏览器：
点击可交互目标（matched=true 的分支，含文本输入框本身）本就不清焦点；
点在"死"的装饰性 chrome 上时不该殃及屏外输入框的焦点。

**应用层配套**（不属于本补丁）：

1. `src/ui/editor.rs` 根元素删除 `onclick { prevent_default() }`（见该处
   注释）——dblclick 合成恢复，composer 焦点由本补丁保护；
2. `src/ui/components/csvtable.rs` 单元格 `onpointerdown` 删除
   `stop_propagation()` / `prevent_default()`——pointerdown 的默认动作
   （`handle_pointerdown`）正是 click_count 的记账点，cancel 它会让双击
   计数永远到不了 2。

## 补丁六：packages/blitz-dom/src/events/pointer.rs（tabindex 元素点击聚焦）

**问题**：`handle_click`（click 默认动作）的 `matched` 判定只认 input /
checkbox / radio / summary / label / a 等交互元素。带 `tabindex` 的普通
div（WYSIWYG 编辑组件的根容器）点击后 `matched=false` → 补丁五的回退
逻辑清焦点（焦点不是 TextInput）→ keydown 的事件链 target=focus_node_id
落回 html 根（无 dioxus id）→ 组件 `onkeydown` 永远收不到键盘。

**修法**：`matched` 循环里增加 `tabindex` 属性检查（HTML 语义：点击
可聚焦元素应聚焦它而非失焦）——命中则 `generate_focus_events(set_focus_to)`
并 `break 'matched true`。

**应用层配套**：`crates/wysiwyg` 的根 div 加 `tabindex=0`（demo 模式）。
任务列表 checkbox **不能**加 `tabindex`：宿主模式下 patch 六会把焦点钉在
方框上，composer 的 onblur→autofocus 收不回来，点完方框键盘整体失效
（实机踩坑，见 WYSIWYG-AUDIT §八 G3）。

**验证**：`tmp/wyshot5.py`（demo 实机：打字/Enter/自动完成/全选删除/
撤销/加粗六项）。

## 补丁七：packages/dioxus-native-dom/src/dioxus_document.rs（IME Commit 转合成 keydown）

**问题**：blitz 的 IME 默认动作只服务 `TextInput`（把提交文本写进
input editor）。非 `<input>` 编辑面（contenteditable 风格组件）收不到
`Ime::Commit`——dioxus 侧原本 `DomEventData::Ime(_) => None` 干脆不暴露，
中文/日文输入整链死亡。

**修法**：`DioxusEventHandler::handle_event` 开头把
`DomEventData::Ime(Commit(text))` **就地改写**为
`DomEventData::KeyDown(BlitzKeyEvent { key: Character(text), is_composing:
false, .. })`——handler-count 快速路径与 `event.name()` 随数据自动变成
keydown，直接进组件已有的 `onkeydown`。Preedit 不暴露（组件自绘组合 UI
或忽略）。提交空串的 Commit 落回原 `None` 分支。

**应用层配套**：非 input 编辑面必须自行启用窗口 IME（winit fork 创建
窗口时固定 `set_ime_allowed(false)`，blitz 唯一开启点只服务 TextInput）——
参照 `src/ui/shell.rs` 的 `enable_ime`（`request_ime_update(Enable)` +
`with_cursor_area()` 能力）。demo 见 `crates/wysiwyg/examples/demo.rs`
（`INFEDITOR_IME=0` 逃生门：GUI 实机测试的合成键会被真 IME 组合缓冲、
Enter 被吃掉）。

## 补丁八：packages/blitz-dom/src/document.rs（图片加载失败集合）

**问题**：blitz 没有 image load 事件。资源失败时 `load_resource` 只把 URL
从 `pending_images` 里移掉就返回，节点永远停在 `ImageData::None`（零尺寸）。
编辑器的 WYS 片段是"先发 `<img>`、由 blitz 异步加载"，拿不到失败信号，
破图位置就永远空白——浏览器会显示 alt 文本，这里什么都没有。

**修法**：`BaseDocument` 增加 `failed_images: HashSet<String>`；
`load_resource` 的 `Err` 分支里，若该 URL 有 `pending_images` 等待者
（即图片请求），把 URL 记进集合；新增 `failed_image_urls()` /
`failed_image_count()` 供宿主查询（集合只增，数量即可作为变更检测）。

**应用层配套**：

1. `src/ui/mdnet.rs` 失败时回调 `handler.bytes(url, 空字节)`——触发
   `ImageHandler` 解码失败，才能走到上面的 Err 分支（只记日志不回调的话
   图片永远停在未加载态）；
2. `src/ui/components/mdws.rs::overlays_frame` 在 `RedrawRequested` 里
   轮询集合，变化时 `wysiwyg::render::set_failed_images(urls)` 并 bump
   `IMG_EPOCH`；
3. `crates/wysiwyg/src/render.rs` 对失败 URL 回落 alt 占位 span（与
   URL 白名单拒绝时同一路径）。

**验证**：`tmp/wys_gui_suite.py --only image_failure_alt`（远程破图 5s 内
出现 alt 文本、文档不置脏）。

## 上游修复后如何摘除

删除 `Cargo.toml` 里的 `[patch."https://github.com/dioxuslabs/blitz"]` 一节
并 `cargo update -p blitz-dom` 即可回到纯 git 依赖；本目录整体删除。

## 补丁九：packages/blitz-shell/src/lib.rs（IME 逃生门，真 TextInput 时代）

**问题**：`ShellProvider::set_ime_enabled(true)` 由 blitz-dom 的
`Node::focus()` 在聚焦**真 TextInput** 时调用（`node.rs` focus 路径）。
查找弹窗换真 `<input>` 后，Tab/点击聚焦输入框都会把真实 Windows IME
请进窗口——真人打字正确（弹窗字段终于可输入中文），但 GUI 自动化
（`INFEDITOR_IME=0` 的测试环境）注入的是 SendInput 虚拟键，真实 IME
把字母截进组合串、WM_CHAR 从此不再出现（实测：Tab 换框后所有按键
`text=None`，输入全丢，test_replace 全线红）。

**修法**：`set_ime_enabled(true)` 读 `INFEDITOR_IME` 环境变量，`0` 时
直接 return（Disable 不拦）——与 winshell `enable_ime` / 组件层显式
Enable 的同名开关语义一致：测试环境全窗禁用 IME，真实会话不受影响。

**验证**：`tests_py/test_replace.py` 全绿（弹窗内 Tab 换框后大小写
字母照常上屏）。

## 补丁十：packages/blitz-dom/src/document.rs（状态快照必须捕获属性）

**问题**：`snapshot_node_state_only` 走 `capture_attrs=false` 路径，产出
`attrs: None` 的 `ServoElementSnapshot`。UA 样式表自带属性选择器
（`input[type=checkbox]`、`hr[size="1"]`…），而失效比对会拿这些选择器去
匹配快照——stylo 的 `ElementSnapshot::get_attr` 对 `attrs: None` 直接
`unwrap()` panic。触发链：文档里只要出现任意 `:hover`/`:active` 规则
（`style_depends_on_state(HOVER)` 变真），悬停一个带属性选择器的元素
（Markdown 任务列表的原生 checkbox）即崩（exit 101，实机实证）。
过去没炸是因为主窗口此前没有任何伪类规则（悬停反馈全靠 Press 信号 +
内联样式）；ui-kit 换 CSS 伪类三态后必然踩中。

**修法**：`snapshot_node_state_only` 与 `snapshot_node` 一样捕获属性
（`capture_attrs=true`）。属性只对单个悬停节点拷贝一次，成本可忽略；
"state is captured at most once, attributes at most once" 的 Gecko 语义
不受影响（`needs_attrs` 的去重逻辑保持不变）。

**验证**：`tests_py/test_wys_all_features.py` 全绿（wys 视图悬停任务
列表 checkbox 不崩）。

## 补丁十一：packages/blitz-dom/src/events/pointer.rs（触摸点按的 slop）

**问题**：`handle_pointermove` 用 2px 的位移阈值判定"拖拽/平移"。手指从
按下到抬起几乎必然漂移 2–5px，于是**每一次触摸点按**都会被判成
`DragMode::Panning`；`handle_pointerup` 的 `do_click = drag_mode == None`
随之抑制 Click——触屏上所有按钮"点不动"，原生 `<button>`、菜单行、
自绘工具栏按钮无一幸免。浏览器正是为此定义了 8–10px 的 touch slop。

**修法**：阈值按指针类型分流——`BlitzPointerId::Finger(_)` 用 8px，
鼠标/笔保持 2px（精确输入没有点按抖动）。平移起点仍取进入 Panning 那一
刻的位置，被吃掉的 slop 位移不会补跳。

同一容差也用在 `handle_pointerdown` 的**双击判定**上（`click_count` 的
两次落点距离）：指尖双击的两次落点天然漂移 3–6px，2px 阈值让触屏上的
双击（选词、CSV 单元格就地编辑）永远凑不齐一对。为此
`handle_pointerdown` 增传 `event.id`（`events/mod.rs` 的调用点同步）。

**配套（应用层，不是补丁）**：需要"绝对可靠点按"的控件继续声明
`touch-action:none`（blitz 见双轴 pan 都被禁就不进 Panning），可滚动容器
不加。两套机制互补：`touch-action:none` 保证 click 不被吞，slop 保证
漏声明的控件也不至于在触屏上变成死键。

**验证**：`INF_TOUCH_SIM=1` 的 `jtap x y 5 3`（带 5×3px 抖动的点按）在
工具栏按钮/菜单行上都能命中命令；`tap` 语义不变。

## 补丁十二：packages/blitz-dom/src/accessibility.rs（未样式化节点的 a11y 遍历）

**问题**：`is_hidden_from_accessibility_tree` 走
`styles.is_display_none()` / `styles.primary()`，两者都对
`ElementStyles::primary` 直接 `unwrap()`。accesskit 树在
`about_to_wait`（winit 空转前）重建，而 DOM 变更（如查找条挂出 →
minimap 高度变化 → `dangerous_inner_html` 重建 SVG 子树）与下一次
样式计算之间存在窗口期——此时节点已挂 ElementData 但**尚无
computed style**，`unwrap()` 直接 panic（exit 101，Ctrl+F 实机复现）。

**修法**：改用 `get_primary()`，无 computed style 视为「不隐藏」
（未样式化的节点不可能是 `display:none`），整条链 `and_then` +
`unwrap_or(false)`，不再 unwrap。风格语义对已样式化节点不变。

**验证**：`tmp/cf_crash.py`（Ctrl+F 后进程存活）；`tests_py/test_find.py`
等 GUI 用例复验。

## 补丁十三：packages/blitz-dom/src/events/pointer.rs（点按判定看"内容有没有滚"）

**问题**：`handle_pointerup` 里 `do_click = drag_mode == DragMode::None`——
只要手指移动越过 slop 进入过 `Panning`，抬指时的 Click 就被整个吞掉，
**不管这次平移到底有没有滚动内容**。于是：

1. 点按时的自然抖动（>slop）被判成拖动 → 触屏上按钮、菜单行、列表行
   "点不动"；
2. 唯一解药是给每个可点元素加 `touch-action:none`（blitz 见双轴都禁就不进
   Panning）——这是一份**逐元素**的清单，新控件漏写就是一颗地雷，而且
   放在可滚动容器里的行没法加（加了就连滚动一起禁掉）；
3. 已滚到边界、或根本没有可滚祖先时，拖动什么也没滚，却同样丢掉 Click。

**修法**：

- `PanState` 增加 `scrolled`，`scroll_chain_by` 真的改动了滚动位置时置位；
- 抬指的判据改成 `DragMode::Panning(state) => !state.scrolled`——平移没
  滚过内容就仍算一次点按；
- 手指的 slop 8px → `FINGER_TAP_SLOP = 12px`（同一常量同时管"进平移"与
  "还算点按"，避免出现"进了平移却没滚、又不给点击"的空档）；
- 从平移里捡回来的点按，**目标取按下时的元素**（`mousedown_node_id`）：
  手指抖十几像素后抬起，用户点的是按下处，不是抬指处碰巧压到的邻居。

**不影响触控滚动**：真正滚动的拖动 `scrolled` 为真，照旧不派发 Click；
应用自己的正文拖动（`ui::touch`，`touch-action:none` 下 blitz 不参与）
用同一个 12px 阈值区分点按与拖动，滚动仍是 1:1 跟手。

**验证**：`INF_TOUCH_SIM` 的 `jtap` 用 10px 级漂移点工具栏/查找条按钮仍
命中；`drag` 滚动照旧（caret 不变）；文本点按落光标。

## 补丁十四：packages/blitz-dom/src/events/{driver,pointer}.rs（按下记账不能受 prevent_default 影响）

**问题（补丁十三之下仍会复发的真根因）**：按下的**内部记账**——
`click_count` 累加、`mousedown_position`、`drag_mode = None`、停掉惯性
滚动——原本写在 `handle_pointerdown` 里，而它是 pointerdown 的**默认动作**。
`handle_pointer_event` 的分发顺序是「先跑页面处理器，没被 cancel 才跑默认
动作」，而本应用为了压掉聚焦跳变，在侧栏行、标签、菜单行、标题栏、
viewport 上都写 `evt.prevent_default()`（`stop_propagation` 同样折算成
cancel）。于是这些元素上：

* `mousedown_position` 永远停在旧值 / `(0,0)`；
* `handle_pointermove` 的 slop 用它当基准 → 手指一动，位移就恒大于 slop
  → 立刻进 `Panning`；
* 一进平移，补丁十三只能在"一格都没滚"时把 Click 捡回来；只要内容跟着
  动了 1–2px（滚动列表里的点按几乎必然如此），Click 依旧被吞
  → **侧栏/菜单这类可滚容器里的行"划一下就被当成滚动、点不进去"**；
* 顺带丢掉两个能力：新按一下**停不住**还在跑的惯性滚动；这些元素上的
  双击/三击也永远数不出第二次。

**修法**：把这笔记账抽成 `note_pointer_down(doc, id, x, y)`，改由事件入口
`driver.rs` 的 `UiEvent::PointerDown` 分支在**分发处理器之前**无条件调用；
`handle_pointerdown` 只保留真正的"默认动作"（命中测试、聚焦、caret、选区）。
语义上与浏览器一致：`preventDefault()` 阻止的是默认**行为**（选中文本、
移动焦点），不影响浏览器自己记的指针状态机。

**注意**：`note_pointer_down` 只在 `UiEvent::PointerDown` 路径调用。若页面
代码（script 特性）自己合成 `DomEventData::PointerDown`，记账会被跳过——
本应用不用 JS，暂无影响；上游若要合入，应放在 `handle_pointer_event`
的 PointerDown 分支里。

**验证**：`tmp/sidebar_tap.py`（侧栏 140 个文件，`overflow-y:auto` 真滚动
容器）三步全过——抖动 5px 点行开文件、手指划 150px 只滚列表不误开、
滚完再点命中新行；`tmp/touch_verify.py` 六例：tap 落光标、drag 滚 250px、
10px 抖动点正文落光标、15px 抖动点查找条按钮仍命中。修复前同一脚本在
第三步失败（`down` 收到、`click` 全无）。

## 配套（应用层）：`.ws-row` 的 `touch-action:none` → `pan-y`

侧栏行铺满整个滚动容器，写 `none` 时 `touch_action_pan_axes` 在目标节点
就否决平移，父容器的原生滚动等于被关掉（划不动，且大划动因
`drag_mode == None` 反而被当成点击开文件）。改成 `pan-y` 后：竖向位移在
slop 内仍是点按，超出 slop 走原生平移滚动，滚过内容就不派发 Click。
`src/ui/components/style.rs` 的 `scrollable_rows_keep_native_panning` 守这条。

## 补丁十五：packages/blitz-dom/src/node/node.rs + packages/blitz-dom/src/layout/paint_tree.rs + packages/blitz-paint/src/render.rs（paint tree 缓存陈旧 id 防护）

**症状**：打开大 JSONL（如 shard_00.jsonl）后拖滚动条/滚轮滚动，命中或
绘制路径随机 panic，崩溃点是 `Node::with(id)`（= `tree().get(id).unwrap()`）
或 `tree()[id]` 索引——都是对**陈旧 NodeId** 解引用。

**根因**：`Node::paint_children` 与 `Node::stacking_context`（hoisted 子节
点列表）都是**上一次 resolve（`build_paint_tree`）留下的缓存**。DOM 增删
（虚拟化视口换行、树折叠/展开）到下一次重建 paint tree 之间存在窗口期：
这期间进来的命中测试（`hit_inner`）或绘制（blitz-paint `draw_children`）
遍历缓存拿到的可能是已经被 slotmap 回收/换代的 id（`NodeId(160v5)` under
`NodeId(157v1)` 这类代数不匹配）。滚动条拖动会高频触发命中 + 换可见行
DOM 增删，正好把窗口期踩实。

**修法**：所有遍历 paint tree 缓存的路径都在解引用前用 `tree().get(id)`
判活，陈旧 id 直接 `continue` 跳过——跳过后绘制少画一个已消失的子节点、
命中少探一个不存在的子节点，下一次 resolve 重建缓存即自愈：

* `node.rs` `hit_inner`：paint_children 循环与**两处 hoisted（z-index）
  子节点循环**都 `tree().get(id)` 判活后才 `hit_inner`（保留
  `INF_HIT_PROBE` 环境变量探针，打出 stale 频率供诊断，默认静默）；
* `paint_tree.rs`：`hoisted_child_position` 对缺失 id 退回零偏移；
  `StackingContext::hoisted_content_bbox` 跳过陈旧子节点（bbox 宁小勿崩）；
* `render.rs` `draw_children`：paint_children 与两处 hoisted 循环同样
  `tree().get(id).is_none() { continue }`，再走原 `tree()[*child_id]` 索引。

**验证**：`tmp/crash_probe.py` 对 shard_00.jsonl 拖滚动条 5 次 + 滚轮 8 次
全存活、0 panic；`INF_HIT_PROBE=1` 下命中路径记录 3845 次 stale 跳过
（同一 id 反复出现，证实缓存失配是常态而非偶发）。

## 补丁十六：packages/blitz-dom/src/events/pointer.rs（右键/中键按下不动选区）

**现场**：MD 预览（主文档原生拖选）里「选中 → 右键菜单 → 复制」永远复制不到
刚选的内容。根因：`handle_pointerdown` 不分按键，右键按下也走
`ClickTarget::SelectableText` 分支——`set_text_selection(点击处, 点击处)` 把
选区**塌缩成光标**，菜单还没打开选区就没了。浏览器行为：右键按下保留现有
选区（点在选区内不塌缩）。

**修法**：`handle_pointerdown` 入口处 `button != Main` 直接 `return`——不改
选区、不抢焦点、不碰 disabled 分支。记账（`note_pointer_down`）在事件入口
无条件执行不受影响；contextmenu 事件在 `handle_pointerup` 的 Secondary
分支派发，也不经过这里。

**验证**：`tmp/mdsel_probe.py`——MD 预览拖选 → 右键：菜单打开时选区高亮
保留（修复前已塌缩），菜单「复制」后剪贴板 = 选中文本。

上游修复后摘除。
