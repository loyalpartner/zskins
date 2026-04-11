# zbar — Wayland 状态栏设计文档

- **日期**：2026-04-11
- **状态**：设计稿（待实现）
- **作者**：lee + Claude（brainstorming session）

## 1. 目标

用 Zed 的 GPUI 框架构建一个 Wayland 状态栏（waybar 风格），同时支持 sway（通过 sway IPC）和实现了 `ext-workspace-v1` 协议的合成器。

项目方针：**MVP 先打通核心链路，架构留好扩展点，后续按模块逐步演进，最终覆盖 waybar 的全部常用功能**。

## 2. 范围

### 2.1 MVP 必做

| 模块            | 来源                                                |
| --------------- | --------------------------------------------------- |
| `workspaces`    | sway IPC 或 ext-workspace-v1（运行时探测）          |
| `clock`         | 本地系统时间                                        |
| `window_title`  | sway IPC（其他合成器下显示空白，作为已知限制）      |

### 2.2 MVP 不做但架构必须支持

未来要逐步加入的模块（按 waybar 常见模块梳理）：
`tray`（StatusNotifierItem）、`battery`、`network`、`pulseaudio/pipewire volume`、`brightness`、`cpu`、`memory`、`temperature`、`disk`、`idle_inhibitor`、`language`、`mpris`、`custom script`、`bluetooth`、`backlight` 等。

未来要扩展的能力：
- 多输出（每个 wl_output 一条 bar）
- 用户配置文件（TOML）
- 主题/样式定制
- 焦点窗口标题非 sway 后端（`ext-foreign-toplevel-list-v1`）
- 鼠标交互（点击、滚轮）扩展到所有模块

### 2.3 明确不做（YAGNI）

- 插件系统（动态加载 .so / Lua 脚本运行时）
- IPC 服务端（让其他工具控制 zbar）
- 嵌入式 webview / HTML 渲染
- 跨平台（X11、macOS、Windows）

## 3. 关键技术决策

### 3.1 GPUI 集成方式

**决策**：直接以 git 依赖方式引入 zed 仓库的 `gpui` crate，**不 fork、不 vendor、不打 patch**。

**理由**：调研发现 GPUI 主线已经原生支持 `wlr-layer-shell`：
- `crates/gpui/src/platform/layer_shell.rs` 提供 `LayerShellOptions`、`Anchor`、`Layer`、`KeyboardInteractivity`
- `WindowKind` 枚举在 wayland feature 下有 `LayerShell(LayerShellOptions)` 变体
- `crates/gpui_linux/src/linux/wayland/window.rs` 已实现 layer surface 创建逻辑
- 官方有 `crates/gpui/examples/layer_shell.rs` 可参考
- 已有外部项目 `andre-brandao/gpui-shell` 验证此路径可行

依赖声明示例：
```toml
[dependencies]
gpui = { git = "https://github.com/zed-industries/zed", features = ["wayland"] }
gpui_platform = { git = "https://github.com/zed-industries/zed" }
```

**风险**：gpui 未发布到 crates.io，构建时需要拉取 zed monorepo（编译时间和磁盘开销显著）；GPUI 在外部使用场景不算成熟生态，遇到 bug 可能要自查或上游提 issue。已接受。

### 3.2 模块系统

**决策**：每个模块是一个独立的 GPUI Entity，自身持有状态和后台任务，挂在 `Bar` entity 的渲染树下。

**理由**：
- 符合 GPUI 的响应式 / Entity-Context 模型
- 模块之间零耦合：状态独立、更新独立、崩溃隔离
- 增量加模块只需新建文件并在 `Bar::new` 中实例化，不动其他代码
- 不需要为 MVP 设计统一 `Module` trait——三个模块还撑不起抽象，等到第 4-5 个模块出现时再提炼

### 3.3 workspace 后端抽象

**决策**：定义一个 `WorkspaceBackend` trait，由 `workspaces` 模块在启动时探测并选择具体实现。

```rust
pub struct WorkspaceState {
    pub workspaces: Vec<Workspace>,
    pub active: Option<WorkspaceId>,
}

pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub active: bool,
    pub urgent: bool,
}

pub enum WorkspaceEvent {
    Snapshot(WorkspaceState),  // 初始或全量刷新
    Updated(WorkspaceState),   // 增量后的新全量
}

pub trait WorkspaceBackend: Send + 'static {
    fn run(
        self: Box<Self>,
        cx: &mut AsyncApp,
    ) -> Task<()>;  // 任务内部把事件 push 给 workspaces 模块
    fn activate(&self, id: WorkspaceId);
}
```

实现：
- `SwayBackend`：通过 `$SWAYSOCK` 连接 sway IPC，订阅 `workspace` 事件
- `ExtWorkspaceBackend`：用独立的 `wayland-client` 连接（不共享 GPUI 内部 wl_display），bind `ext_workspace_manager_v1`，处理 manager/group/workspace 三层事件

事件传递：模块持有一个 `WeakEntity<WorkspacesModule>`，后台任务通过 `weak.update(cx, |module, cx| { module.apply(event); cx.notify(); })` 推送事件。

### 3.4 后端探测

**决策**：启动时按优先级探测，第一个可用的胜出。

```text
1. 环境变量 $SWAYSOCK 存在 且 socket 可连 → SwayBackend
2. wl_registry 通告中包含 ext_workspace_manager_v1 → ExtWorkspaceBackend
3. 都没有 → workspaces 模块降级为空 div，bar 仍然启动
```

探测和后端构造发生在主线程的同步代码中，结果通过 channel 或直接 move 传给模块。

### 3.5 配置

**决策**：MVP 把所有配置硬编码在源码常量里（`theme.rs` + 模块构造参数）。**不引入 TOML / serde / dirs**。

**理由**：YAGNI。MVP 阶段配置只有 bar 高度、颜色、模块顺序，硬编码完全够；过早引入配置文件会拖慢核心开发并且配置 schema 还会反复改。配置文件留到至少 5 个模块上线后再做。

### 3.6 异步运行时

**决策**：复用 GPUI 自带的 executor，**不引入 tokio**。

- 后台任务用 `cx.background_spawn(async move { ... })`
- UI 协作任务用 `cx.spawn(async move |this, cx| { ... })`
- IO：`smol`、`async-net`、`async-io` 系列与 GPUI executor 兼容；对 sway IPC 这种 unix socket，可以用 `smol::net::unix::UnixStream`
- 阻塞 IO：用 `cx.background_executor().spawn(async move { blocking::unblock(...) })`

### 3.7 wayland 客户端

- `ext_workspace_v1` 后端使用独立的 `wayland-client` 连接（GPUI 不暴露其内部 `wl_display`）
- 使用 `wayland-client` + `wayland-protocols` crate；如果 `ext-workspace-v1` 还未进入 stable `wayland-protocols`，则把协议 XML vendored 到本仓库 `protocols/` 目录并用 `wayland-scanner` 在 `build.rs` 中生成绑定
- 后端运行在自己的线程或 GPUI 后台任务中，事件循环用 `EventQueue::dispatch_pending` + 异步 fd 监听

## 4. 仓库结构

```
zbar/
├── Cargo.toml
├── build.rs                       # 协议 XML → Rust 绑定（如需要）
├── protocols/
│   └── ext-workspace-v1.xml       # 如果 wayland-protocols crate 还没收录
├── docs/
│   └── superpowers/
│       └── specs/
│           └── 2026-04-11-zbar-design.md
├── src/
│   ├── main.rs                    # 入口：创建 App、构造 Bar、open layer-shell window
│   ├── bar.rs                     # Bar entity：三段布局，持有 module entities
│   ├── theme.rs                   # 颜色、字号、bar 高度、间距常量
│   ├── modules/
│   │   ├── mod.rs                 # 模块 re-exports
│   │   ├── clock.rs               # ClockModule entity
│   │   ├── workspaces.rs          # WorkspacesModule entity + Workspace 类型
│   │   └── window_title.rs        # WindowTitleModule entity
│   └── backend/
│       ├── mod.rs                 # WorkspaceBackend trait + WorkspaceEvent / WorkspaceState 类型
│       ├── detect.rs              # 启动期后端探测逻辑
│       ├── sway.rs                # SwayBackend：sway IPC 协议实现
│       └── ext_workspace.rs       # ExtWorkspaceBackend：ext-workspace-v1 协议实现
└── tests/
    └── fixtures/
        ├── sway_workspace_event.json
        └── ext_workspace_events.bin
```

## 5. Bar 窗口与布局

### 5.1 layer-shell 配置

```rust
WindowKind::LayerShell(LayerShellOptions {
    namespace: "zbar".to_string(),
    layer: Layer::Top,
    anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
    exclusive_zone: Some(px(BAR_HEIGHT)),
    keyboard_interactivity: KeyboardInteractivity::None,
    ..Default::default()
})
```

- `BAR_HEIGHT = 32.0` (px)
- 顶部贴边，水平撑满
- `exclusive_zone` 让合成器把其他窗口往下挤
- 键盘交互关闭（bar 不需要焦点）

`WindowOptions`：
- `titlebar: None`
- `window_background: WindowBackgroundAppearance::Transparent`
- `app_id: Some("zbar".to_string())`
- `window_bounds`: 高度 32px，宽度由 anchor 决定

### 5.2 三段布局

```rust
div()
    .size_full()
    .flex().items_center()
    .px_2()
    .bg(theme::BG)
    .text_color(theme::FG)
    .text_size(theme::FONT_SIZE)
    .child(
        div().flex_1().flex().items_center().gap_1()
            .child(self.workspaces.clone())
    )
    .child(
        div().flex_1().flex().items_center().justify_center()
            .child(self.window_title.clone())
    )
    .child(
        div().flex_1().flex().items_center().justify_end()
            .child(self.clock.clone())
    )
```

每段 `flex_1` 等宽分配，避免中间被两侧挤偏。

### 5.3 模块视觉规范（MVP）

- **workspaces**：横向圆角小方块，每个 workspace 一个，活动态用强调色背景，普通态透明背景，hover 半透明高亮；点击触发 `activate`
- **clock**：纯文本 `HH:MM:SS`，每秒刷新
- **window_title**：纯文本，超长截断（`text-overflow: ellipsis` 风格，GPUI 用 `truncate()`），最大宽度限制为容器宽度

## 6. 数据流

```
[sway socket / wl_display]
        │
        ▼
   Backend task (cx.background_spawn)
        │  push WorkspaceEvent via WeakEntity::update
        ▼
   WorkspacesModule entity 状态更新
        │  cx.notify()
        ▼
   Bar render → GPUI frame
```

时钟模块没有外部数据源，直接 `cx.spawn` 一个 `loop { timer(1s).await; cx.notify(); }`。

窗口标题模块（sway 模式）和 workspaces 共用一个 sway IPC 连接？**MVP 不共用**：每个模块独立连接，简单且隔离。等连接数变成问题再合并。

## 7. 错误处理与降级

| 场景                            | 行为                                                              |
| ------------------------------- | ----------------------------------------------------------------- |
| 后端探测全部失败                | 模块渲染为空 div；bar 正常启动；stderr 打印一行警告               |
| 后端运行时断连（socket 关闭）   | 模块状态置空；后台任务指数退避重连（1s → 2s → 5s → 上限 30s）     |
| 后端发送畸形数据                | 当前事件丢弃，记录 warn，循环继续                                 |
| GPUI 窗口创建失败               | panic 并退出（不可恢复，最早期失败）                              |
| 模块 render 内部 panic          | 由 GPUI panic hook 处理；不主动 catch                             |

**原则**：bar 一旦启动就要持续显示，不允许因为某个数据源出问题而崩溃或退出。

## 8. 测试策略

### 8.1 单元测试

- `backend/sway.rs`：JSON 消息解析、订阅消息构造，使用 `tests/fixtures/` 下的样本
- `backend/ext_workspace.rs`：事件序列 → state 状态机的纯函数部分（不依赖真实 wayland 连接）
- `modules/workspaces.rs`：`apply(event)` 后状态正确（不依赖 GPUI 渲染）

### 8.2 不写的测试

- GPUI 渲染层单测（GPUI 测试基础设施薄弱，性价比低）
- 实际 wayland 连接的集成测试（需要真实合成器，留给手动测试）

### 8.3 手动验收（每次大改后）

- sway 下：`cargo run`，观察 bar 出现，workspace 列表正确，切换 workspace 高亮跟随，点击切换生效，焦点窗口标题更新，时钟刷新
- 任一实现 `ext-workspace-v1` 的合成器下：同上但 `window_title` 模块空白

## 9. MVP 完成标准

1. `cargo run` 在 sway 下启动顶部 bar，左侧显示当前 sway workspace 列表（编号 + 活动高亮），点击可切换
2. 同一份二进制在任一实现 `ext-workspace-v1` 的合成器下能跑，左侧正确显示该合成器的 workspace
3. 中间显示焦点窗口标题（sway-only；非 sway 合成器下空白且不报错）
4. 右侧显示 `HH:MM:SS` 实时刷新
5. 杀掉 sway 模拟断连，bar 不崩溃，10 秒内自动重连恢复

## 10. 已知限制（MVP 阶段）

- `window_title` 模块在非 sway 合成器下空白（计划：后续加 `ext-foreign-toplevel-list-v1` backend）
- 只在主输出显示一条 bar（计划：后续监听 `wl_output` 增删，每个输出一条）
- 不支持配置文件（计划：5 个模块以上时引入 TOML）
- 不支持自定义主题（计划：与配置文件一起做）
- 不支持鼠标滚轮、右键菜单（计划：模块系统稳定后扩展）

## 11. 后续演进路线（草图）

| 阶段 | 内容                                                        |
| ---- | ----------------------------------------------------------- |
| v0.2 | 多输出支持；TOML 配置；主题                                 |
| v0.3 | 第 4-5 个模块（clock 之外的简单模块：battery, volume）      |
| v0.4 | 提炼 `Module` trait；模块顺序与左/中/右配置                 |
| v0.5 | tray（最复杂的模块）                                        |
| v0.6 | network、bluetooth、mpris                                   |
| v0.7 | custom script 模块                                          |
| 长远 | 完成 waybar 全部常用模块                                    |
