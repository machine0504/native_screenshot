# native_screenshot — Snipaste 式截图行为规范 (SPEC)

本文件是 Windows / macOS / Linux 三端原生实现必须共同遵守的行为契约。三端 UI 各自原生绘制，但交互、工具集、快捷键与输出结果必须一致。

## 1. 通道契约 (Dart <-> Native)

MethodChannel 名称：`native_screenshot`

| Method | 入参 | 返回 | 说明 |
| --- | --- | --- | --- |
| `captureRegionOnDesktopOverlay` | 无 | `Uint8List?` (PNG)，取消为 `null` | 进入全屏遮罩，框选 + 标注，确认后返回选区 PNG，并同时写入系统剪贴板 |
| `captureFullScreen` | `{displayId:int}` | `Uint8List?` (PNG) | 整屏截图（不弹遮罩） |
| `copyImageToClipboard` | `Uint8List` (PNG) | `bool` | 将 PNG 写入系统剪贴板（图片格式） |

调用方（`desktop_snip_flow.dart`）负责在调用前隐藏/最小化应用窗口、调用后恢复。原生侧只负责"冻结快照 + 遮罩 + 标注 + 输出"。

## 2. 会话阶段 (Phases)

1. `Selecting`（框选）：鼠标拖拽出矩形选区。
2. `Adjusting`（调整）：选区已确定，显示 8 个手柄 + 工具条；可拖动手柄缩放、拖动选区内部移动、点击选区外重新框选。
3. `Annotating`（标注）：选中某个标注工具后进入；在选区内绘制标注。

注：`Adjusting` 与 `Annotating` 共用同一个工具条，工具条随选区位置浮动。

## 3. 冻结快照 (Frozen snapshot)

- 进入会话时，一次性抓取"整个虚拟桌面"（所有显示器并集）的位图，之后遮罩绘制的全部是这张**静态**位图（不是实时桌面），避免闪烁/穿帮。
- 必须按物理像素抓取，正确处理多显示器与 HiDPI 缩放，保证 1:1 清晰。
- 放大镜、取色、马赛克采样、最终裁剪输出，全部基于这张冻结位图。

## 4. 框选阶段 UI

- 选区外覆盖 45% 黑色半透明遮罩；选区内显示原始（未变暗）像素。
- 选区边框：1.5~2px 主题蓝 (`#1E90FF` 风格)。
- 尺寸标签：选区左上角上方小药丸，显示 `宽 × 高`（物理像素）。
- 放大镜（仅 Selecting 阶段，跟随光标）：
  - 放大镜内显示光标周围像素放大（约 8x），中心十字。
  - 下方显示当前像素颜色 + 左侧色块；首行为颜色值（按当前格式），次行为提示「Shift 切换  C 复制」。
  - **Shift 循环切换颜色格式**：`#RRGGBB` → `rgb(r, g, b)` → `rgba(r, g, b, 255)` → `hsv(h, s%, v%)` → 回到 Hex。
  - **C 直接复制**当前光标处颜色值（按当前格式，写入剪贴板为文本）。
- 8 个调整手柄：四角 + 四边中点，白底蓝框小方块；命中容差约 8~10px。
- 拖动选区内部可整体移动选区。
- 点击选区外（Adjusting 阶段）重新开始框选。
- **窗口自动识别**：尚未框选时，鼠标悬停会自动高亮光标下最上层窗口的区域（截屏同一时刻枚举窗口矩形，Windows 用 DWM 真实可见边界）；**单击即选中**该窗口区域，**拖拽**则为自由框选。

### 光标反馈（随状态变化）

| 状态 | 光标 |
| --- | --- |
| 框选阶段（含拉框） | 十字 Crosshair |
| 悬停/拖动四角手柄 | 斜向缩放 ↖↘ / ↗↙（NwseResize / NeswResize） |
| 悬停/拖动四边中点手柄 | 横/纵缩放（EwResize / NsResize） |
| 选区内部（无标注工具） | 移动 Move；拖动移动选区时为抓取 Grabbing |
| 选区内 + 已选标注工具 | 十字（准备绘制） |
| 悬停工具条 | 手型 Pointer |
| 文字输入中 | 编辑框内为文本光标 I-beam（Text），框外为默认 Default |

## 5. 标注工具集 (Tools)

所有形状默认开启抗锯齿。

| 工具 | 行为 |
| --- | --- |
| 矩形 rect | 拖拽描边矩形；按 Shift 约束为正方形 |
| 椭圆 ellipse | 拖拽描边椭圆；按 Shift 约束为正圆 |
| 箭头 arrow | 拖拽，终点带实心箭头 |
| 直线 line | 拖拽直线 |
| 画笔 pen | 自由手绘折线（平滑） |
| 马克笔 marker | 半透明高亮（约 40% alpha），较粗 |
| 马赛克 mosaic | 沿路径对**冻结位图**按块取平均色填充（真实像素化，非随机色） |
| 文字 text | 点击落点进入文字编辑：支持中文 IME（**候选条悬浮在遮罩之上**）、多行（Shift+Enter 换行）、插入光标（方向键 / Home / End 移动，Backspace / Delete 删除）、**选区**（Shift+方向键 / 编辑框内拖拽选择 / 双击全选 / Ctrl/Cmd+A）、复制剪切粘贴（Ctrl/Cmd + C/X/V，粘贴/输入替换选区）、闪烁光标；颜色随调色板实时变化。提交后绘制为文字。**编辑态显示变换框**（见 §5.1）；**鼠标悬停已有文字显示虚线框（提示可点击编辑）；单击已有文字即进入编辑，拖拽则移动** |
| 序号 serial | 点击放置自增数字圆形徽标（1,2,3...） |
| 橡皮擦 eraser | 点击命中最近的一条标注并删除它 |

### 5.1 文字编辑态 / 变换框 (Text editing gizmo)

文字处于编辑态（正在输入或单击进入）时，文字框周围显示一个跟随旋转的变换框：

- **边框**：主题蓝细线矩形，随旋转角度倾斜。
- **缩放手柄**：四角 + 四边中点（共 7 个白底蓝框小方块），**右上角除外**。从中心对称缩放字号；命中后按指针到中心的距离比例调整字号。
- **关闭按钮**：位于**右上角**，红底白「✕」圆点。点击删除该文字（新建未提交则丢弃）并退出编辑。
- **旋转手柄**：顶边中点正上方一段引线 + 白底蓝框圆点。拖拽绕中心旋转；接近 90° 整数倍时吸附，便于摆正。
- **移动**：拖拽边框线（非手柄处）平移文字框；框内拖拽用于选择文字。
- 文字内容、光标、选区、IME 候选定位均随该角度一并旋转。

## 6. 工具条控件

- 工具按钮：矩形 / 椭圆 / 箭头 / 直线 / 画笔 / 马克笔 / 马赛克 / 文字 / 序号 / 橡皮擦
- 颜色板：红 `#FF3B30`、橙 `#FF9500`、黄 `#FFCC00`、绿 `#34C759`、蓝 `#007AFF`、紫 `#AF52DE`、白 `#FFFFFF`、黑 `#000000`
- 线宽：细 (2) / 中 (4) / 粗 (6)
- 撤销 undo / 重做 redo
- 操作：取消（关闭，不上传，红 ✗）/ 保存 / 确定（确认并输出：写剪贴板 + 返回 PNG 供宿主上传到提问框，绿 ✓）
- 工具条出现在选区下方；若下方空间不足则放到选区上方；左右钳制在屏幕内。
- **外观（与主流截图工具对齐）**：深色半透明圆角外壳（炭灰 ~`rgba(43,45,52)`）、浅灰单色图标；选中项为浅色高亮药丸 + 主题蓝图标/描边；分组之间有竖向浅色分隔线；底部投影。

## 7. 撤销 / 重做

- 每完成一笔标注（含文字提交、序号放置、橡皮擦删除）压入 undo 栈，清空 redo 栈。
- undo：从标注列表弹出最后一项压入 redo 栈。
- redo：反向。

## 8. 键盘快捷键

| 键 | 行为 |
| --- | --- |
| Esc | 取消整个会话（返回 null）。文字输入中：若正在 IME 拼字（有 preedit）仅取消本次拼字（通常由系统输入法直接消化），否则同样取消整个会话。仅取消当前文字框可用右键或变换框右上角「✕」 |
| Enter / 数字键盘 Enter | 文字输入中=提交文字；否则=确定（确认输出，写剪贴板，结束会话返回 PNG） |
| Shift + Enter | （仅文字输入中）插入换行 |
| 方向键 / Home / End | （仅文字输入中）移动光标；按住 Shift 扩选 |
| Backspace / Delete | （仅文字输入中）删除选区，或光标前 / 后一个字符 |
| Ctrl/Cmd + A | （仅文字输入中）全选 |
| Ctrl/Cmd + C / X | （仅文字输入中）复制 / 剪切选区 |
| Ctrl/Cmd + V | （仅文字输入中）在光标处粘贴（替换选区） |
| Ctrl/Cmd + Z | 撤销 |
| Ctrl/Cmd + Y（或 Cmd+Shift+Z）| 重做 |
| Ctrl/Cmd + S | 保存为文件（弹原生保存对话框） |
| Shift | （仅框选前）循环切换放大镜的取色格式 |
| C | （仅框选前）复制光标处颜色值到剪贴板 |
| 右键 | 框选阶段=取消会话；标注阶段=退出当前标注工具/取消 |

## 9. 输出 (Render & output)

1. 以选区矩形从冻结位图裁剪出原始像素。
2. 在其上按"插入顺序"绘制全部标注（马克笔半透明叠加、马赛克为像素化块）。
3. 编码为 PNG。
4. 确定（原"复制"）：写入系统剪贴板（图片格式），并通过通道返回 PNG 字节（宿主据此上传到提问框）。
5. 保存：弹原生保存对话框写文件，同时也返回 PNG 字节。
6. 取消：返回 `null`，不写剪贴板。

## 10. 坐标系约定

- 选区、标注几何统一以"虚拟桌面物理像素"为基准存储（叠加层与冻结位图同坐标系），输出时直接相对选区原点偏移即可，避免逻辑像素/物理像素换算误差。

## 11. 实现架构（重要）

整套 UI 由跨平台 Rust 程序 `rust_screenshot`（源码在本插件 `rust_screenshot/`，winit + tiny-skia）**一次写成**，编译为独立的辅助可执行文件随应用分发。`native_screenshot` 是一个 **Flutter plugin**（自带跨平台构建集成），Dart 侧 `ScreenshotController.captureRegionOnDesktopOverlay()` 以子进程方式启动该程序并读取其产出的 PNG；对外推荐使用更高层的 `NativeScreenshot.captureRegion()`（额外封装了屏幕录制权限检查与窗口最小化/恢复）。

进程间约定：助手在 stdout 最后一行输出 `OK <png路径>` / `CANCELLED` / `ERROR <msg>`；Dart 侧据此读取临时 PNG 文件（读后删除）。

辅助程序定位顺序：① 环境变量 `NATIVE_SCREENSHOT_HELPER`；② 与主程序同目录（发布包）；③ 开发期在工程树中向上查找 `package/native_screenshot/rust_screenshot/target/{release,debug}/`。

## 12. 各平台构建与随包分发（已下沉到本插件）

- Windows：插件 `windows/CMakeLists.txt` 在构建时 `cargo build --release` 并把 `rust_screenshot.exe` 安装到 app 同目录（被 Flutter 构建自动接管，宿主 App 零配置）。
- Linux：插件 `linux/CMakeLists.txt` 同理，把 `rust_screenshot` 拷到 bundle 根（与可执行文件同级）。
- macOS：因 CocoaPods 无法把可执行文件放进 `*.app/Contents/MacOS/`，由宿主 App 保留一个 Run Script 构建阶段，指向插件 `macos/scripts/embed_rust_screenshot.sh`，由其 `cargo build` 并把 `rust_screenshot` 拷到 `*.app/Contents/MacOS/`、复用宿主签名身份签名。

构建机需安装 Rust 工具链（`cargo` 在 PATH 中）。插件 `rust_screenshot/.cargo/config.toml` 配了 rsproxy 国内镜像，仅作用于该 crate。

> 宿主 App 的接入步骤（依赖、调用、macOS 权限/脚本配置、验证清单）见同目录 `INTEGRATION.md`。

## 13. 平台注意事项（须在对应系统上验证）

- macOS：助手位于宿主 `*.app/Contents/MacOS/`、复用宿主 bundle 身份运行，屏幕录制权限随宿主（系统"屏幕录制"里只有宿主一个条目，不再单独申请）。分发/公证时用正式签名身份签名并启用 hardened runtime。
- macOS 输入法候选条：遮罩处于 `CGShieldingWindowLevel()+1` 顶层，系统输入法会把候选条挂在「聚焦文本客户端上报的窗口层级之上」。winit 的文本视图实现了 `NSTextInputClient` 但**缺少可选的 `windowLevel` 方法**，导致系统按默认低层级摆放候选条而被遮罩盖住。修复：运行时给 winit 视图类补上 `windowLevel`（返回当前窗口实际层级，见 `macos.rs`），候选条即悬浮在遮罩之上。文字输入期间仍把遮罩降到 `NSStatusWindowLevel`(25)（高于 Dock/菜单栏，且数值不溢出），候选条稳定落在其上。
- Linux：X11 下为真·全屏透明覆盖层，体验与 Windows 一致；Wayland 无法任意定位窗口、且抓屏走 portal，可能弹系统授权框，覆盖层行为受合成器限制。IME 候选条的层级机制为 macOS 专有；X11/Wayland 由各自输入法框架处理。
- macOS/Linux 的抓屏经由 `xcap` crate，需在对应系统上 `cargo build` 验证（若 xcap 版本 API 有出入按编译报错微调 `desktop/rust_screenshot/src/capture.rs`）。
