# RTT2

RTT2 是面向 ARTINX Kit-C 和 Kit-miao 的 Windows 桌面调试工具。它通过 probe-rs 连接 CMSIS-DAP 或 J-Link 探针，从 Keil .map 文件定位 SEGGER RTT 控制块，显示 5 ms 遥测，并通过 RTT 发送一次性命令。GUI 和常驻 CLI 接入同一个公共后端，共用探针连接；CLI 可独立工作。

## 功能

- 实时显示 debug_info.status（32 位二进制、十六进制、十进制）和 value1 至 value13 的数值。
- 多个独立曲线窗口共用主窗口接收的数据；支持 CSV 记录、回放和单次发射记录。
- 常驻 CLI 支持 connect、send、catch、wait、status、disconnect、quit；命令按会话和序号匹配执行回执。
- 遥测含序号、板端时间戳和 CRC16；坏帧与丢帧分别统计。

RTT2 目前仅支持 Windows。固件在独立的 ARTINX 工程中维护；本仓库只包含上位机代码。使用前请编译并烧录支持 RTT2 协议的固件。

## 快速开始

1. 从 [v1.3.0 Release](https://mirrors.sustech.edu.cn/git/artinx_eee/artinx_core/rtt2/-/releases/v1.3.0) 下载 rtt2v1.3.0.exe，连接开发板和调试探针。
2. 双击程序，选择芯片与**该板卡的 Keil 编译输出目录**。目录中需有包含 _SEGGER_RTT 符号的 .map 文件。
3. 点击“启动 RTT 监控”。主窗口右侧显示状态和原始变量；点击“新建曲线窗口”，在单列列表中选择变量，再创建独立曲线页面；可同时打开多个窗口。
4. 根据需要开始 CSV 记录、加载 CSV 回放，或点击“记录本发”保存一次发射时的快照。

| 板卡 | 芯片型号 | 工程输出目录示例 |
| --- | --- | --- |
| Kit-C | STM32F407IGHx | artinx-frame-2027/bsp/cubemx/MDK-ARM/Kit-C |
| Kit-miao | STM32H723VG | artinx-frame-2027/bsp/cubemx/MDK-ARM/kit-miao |

程序会拒绝包含多个不同 RTT 地址的目录；请选具体板卡的输出目录。若连接到 RTT 控制块后 5 秒内没有有效遥测，程序会断开并提示检查固件或复位开发板。

### 曲线查看

- 每次新建窗口时变量均为未选中，单列显示 value1–value13，不沿用上次选择；图表随窗口缩放自动适配。
- 默认跟随最新数据，时间跨度为 10 秒；拖动平移、Ctrl + 滚轮缩放、右键拖动框选放大后切换为“自由查看”。
- **回到最新**：只恢复时间轴跟随，保留手动调整的时间跨度和纵轴范围。
- **重置视图**：恢复 10 秒时间跨度并自动适配纵轴。
- **清除曲线**：只清除当前窗口的显示历史，其他窗口和 CSV 记录不受影响。

新 CSV 的变量表头为 value1–value13，按下位机字段顺序对应；旧 CSV 回放保留文件原有列名。

### 常驻 CLI

双击 EXE 打开 GUI；在 CMD、PowerShell 中运行 EXE 进入 `rtt2>`。也可用 `rtt2.exe gui` 或 `rtt2.exe cli` 明确指定。以下假定将 EXE 命名为 `rtt2.exe` 并放到当前目录或 PATH 中；下载的 `rtt2v1.3.0.exe` 用法相同。

```text
> rtt2
rtt2> connect --project "D:\board output\kit-miao" --chip STM32H723VG
rtt2> catch 10 test.csv
rtt2> send 0
rtt2> status
rtt2> wait
rtt2> disconnect
rtt2> quit
```

- 首次连接指定芯片和包含 `.map` 文件的具体板卡输出目录。连接成功后保存配置，下次可直接 `connect`。Kit-C 的芯片为 `STM32F407IGHx`，Kit-miao 为 `STM32H723VG`。
- `catch <秒数> <文件名>` 从现在开始后台采集，期间仍可 `send`；`wait` 等待文件写完。时长支持小数，范围为大于 0 至 86400 秒。CSV 包含序号、板端时间戳、status、status_hex 和 value1–value13；已有文件不会覆盖。
- `status` 返回连接情况、最新一帧、CRC/丢帧统计和采集进度。`cancel` 结束本终端的采集并保留部分文件。
- `gui` 打开接入当前连接的图形界面。关闭任一界面不会断开其他界面仍在使用的连接；`disconnect` 会明确断开所有界面共用的板卡连接。最后一个界面退出后，后端自动释放探针并退出。
- `quit`、输入结束或 Ctrl+C 会结束当前终端；由它发起的未完成采集会保存为部分文件。脚本请先 `wait` 再 `quit`。

当前固件命令为 `send 0`（无参数 PING）和 `send 1 1.0 2.0`（ECHO，最多 12 个有限 float32）。GUI 内的命令 Shell 保留相同的 `send` 用法。板端回执超时为 2 秒，超时报告“结果未知”，不自动重发。

AI／脚本使用同一常驻终端，通过标准输入连续发送命令、读取标准输出。`cli --json` 输出逐行 JSON；输入重定向时自动使用 JSON 模式。**v1.2 的外部 `rtt2 send ...`、`rtt2 status ...` 入口已移除**，请改为终端内命令。

完整参数、脚本示例和退出行为见 [CLI 使用说明](docs/CLI.md)。

## 从源码构建

安装支持 Rust 2024 edition 的 Rust 工具链和 Windows C++ 构建工具，在仓库根目录运行：

~~~powershell
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
~~~

产物是 target/release/rtt2.exe。可运行 cargo run --example stability -- 'C:\path\to\Kit-C' 30 STM32F407IGHx 做实板稳定性观察。界面刷新间隔默认 5 ms；改变它不会改变固件采样周期。

协议字段、CRC 和兼容性见 [协议文档](docs/PROTOCOL.md)。发布版本记录见 [CHANGELOG.md](CHANGELOG.md)，参与开发见 [CONTRIBUTING.md](CONTRIBUTING.md)。本仓库目前**未附带开源许可证**。
