# RTT2

[![Release](https://github.com/hyj0824/rtt2/actions/workflows/release.yml/badge.svg)](https://github.com/hyj0824/rtt2/actions/workflows/release.yml)

RTT2 是面向 ARTINX Kit-C 和 Kit-miao 的跨平台桌面调试工具，支持 Windows、Linux 和 macOS。它通过 probe-rs 连接 CMSIS-DAP 或 J-Link 探针，从固件 ELF（Keil `.axf`、armgcc/armclang `.elf`、IAR `.out`）的 `_SEGGER_RTT` 符号定位 SEGGER RTT 控制块，显示 5 ms 遥测，并通过 RTT 发送一次性命令。GUI 和常驻 CLI 接入同一个公共后端，共用探针连接；CLI 可独立工作。

## 功能

- 实时显示 debug_info.status（32 位二进制、十六进制、十进制）和 value1 至 value13 的数值。
- 多个独立曲线窗口共用主窗口接收的数据；支持 CSV 记录、回放和单次发射记录。
- 常驻 CLI 支持 connect、send、catch、wait、status、disconnect、quit；命令按会话和序号匹配执行回执。
- 遥测含序号、板端时间戳和 CRC16；坏帧与丢帧分别统计。

RTT2 支持 Windows、Linux 和 macOS。固件在独立的 ARTINX 工程中维护；本仓库只包含上位机代码。使用前请编译并烧录支持 RTT2 协议的固件。Linux 上访问调试探针通常需要 udev 规则或相应的用户组权限；macOS 使用 CMSIS-DAP 通常无需额外驱动，J-Link 请安装 SEGGER 软件。

## 快速开始

1. 从 [Releases](https://github.com/hyj0824/rtt2/releases) 下载对应平台的压缩包（Windows、Linux、macOS，各有 x86_64 与 arm64），解压后连接开发板和调试探针。Linux/macOS 首次运行前需 `chmod +x rtt2`；macOS 未签名版本可能需要在“系统设置 → 隐私与安全性”中允许，或执行 `xattr -d com.apple.quarantine rtt2`。
2. 打开 GUI（Windows 双击程序；Linux/macOS 运行 `./rtt2 gui`），选择芯片和**该板卡的固件输出目录**（也可直接选固件文件）。程序读取固件 ELF 中的 `_SEGGER_RTT` 符号；Keil `.axf`、armgcc/armclang `.elf`、IAR `.out` 都支持。也可在“RTT 地址”中直接填写控制块地址，或把地址和目录都留空，由后端扫描 RTT 内存。
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

Windows 双击 EXE 打开 GUI；在 CMD、PowerShell 中运行 EXE 进入 `rtt2>`。Linux 和 macOS 在终端运行 `./rtt2` 进入 CLI，运行 `./rtt2 gui` 打开 GUI；把可执行文件放到 PATH 后可直接使用 `rtt2`。也可用 `rtt2 cli` 明确指定 CLI。以下假定将可执行文件命名为 `rtt2`（Windows 为 `rtt2.exe`）；从 Release 压缩包解压出的文件用法相同。

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

Linux/macOS 的路径写法不同，例如：

```text
rtt2> connect --project "/home/me/firmware/bsp/cubemx/MDK-ARM/kit-miao" --chip STM32H723VG
```

- 首次连接指定芯片和具体板卡输出目录或固件文件（`.elf`/`.axf`/`.out`），由程序读取 `_SEGGER_RTT` 符号。连接成功后保存配置，下次可直接 `connect`。Kit-C 的芯片为 `STM32F407IGHx`，Kit-miao 为 `STM32H723VG`。配置位置：Windows 为 `%LOCALAPPDATA%\RTT2\connection.json`，macOS 为 `~/Library/Application Support/RTT2/connection.json`，Linux 为 `$XDG_CONFIG_HOME/RTT2/connection.json`（默认 `~/.config/RTT2/`）。也可用 `--address 0x20009fa4` 直接指定地址（适合 ELF 已 strip 的情况），或只给 `--chip` 由后端扫描 RTT 内存。
- `catch <秒数> <文件名>` 从现在开始后台采集，期间仍可 `send`；`wait` 等待文件写完。时长支持小数，范围为大于 0 至 86400 秒。CSV 包含序号、板端时间戳、status、status_hex 和 value1–value13；已有文件不会覆盖。
- `status` 返回连接情况、最新一帧、CRC/丢帧统计和采集进度。`cancel` 结束本终端的采集并保留部分文件。
- `gui` 打开接入当前连接的图形界面。关闭任一界面不会断开其他界面仍在使用的连接；`disconnect` 会明确断开所有界面共用的板卡连接。最后一个界面退出后，后端自动释放探针并退出。
- `quit`、输入结束或 Ctrl+C 会结束当前终端；由它发起的未完成采集会保存为部分文件。脚本请先 `wait` 再 `quit`。

当前固件命令为 `send 0`（无参数 PING）和 `send 1 1.0 2.0`（ECHO，最多 12 个有限 float32）。GUI 内的命令 Shell 保留相同的 `send` 用法。板端回执超时为 2 秒，超时报告“结果未知”，不自动重发。

AI／脚本使用同一常驻终端，通过标准输入连续发送命令、读取标准输出。`cli --json` 输出逐行 JSON；输入重定向时自动使用 JSON 模式。**v1.2 的外部 `rtt2 send ...`、`rtt2 status ...` 入口已移除**，请改为终端内命令。

完整参数、脚本示例和退出行为见 [CLI 使用说明](docs/CLI.md)。

## 从源码构建

安装支持 Rust 2024 edition 的 Rust 工具链。Windows 需要 C++ 构建工具；macOS 需要 Xcode Command Line Tools；Linux 的 GUI 构建需要常见的图形开发库（如 libxkbcommon、libwayland、libGL 及 pkg-config），CLI-only 构建不需要这些 GUI 开发库。在仓库根目录运行：

~~~sh
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
~~~

默认构建包含 GUI。不需要 GUI 的环境可以只构建 CLI，Cargo 不会引入或编译 eframe、egui、rfd、winit 等 GUI 依赖：

~~~sh
cargo test --locked --no-default-features
cargo build --release --locked --no-default-features
~~~

CLI-only 构建中 `rtt2` 始终进入常驻 CLI，`rtt2 gui` 和 CLI 内的 `gui` 命令会提示此构建未包含 GUI。

Windows 产物是 `target/release/rtt2.exe`，Linux/macOS 产物是 `target/release/rtt2`。Linux 使用探针前请按 [probe-rs 探针配置](https://probe.rs/docs/getting-started/probe-setup/) 安装 udev 规则（或 J-Link 的 SEGGER 规则）并重新插拔设备；无权限时 `probe-rs` 会报告找不到调试器。可运行 `cargo run --example stability -- '/path/to/Kit-C' 30 STM32F407IGHx` 做实板稳定性观察（Windows 路径示例：`'C:\path\to\Kit-C'`）。界面刷新间隔默认 5 ms；改变它不会改变固件采样周期。

Linux 和 macOS 的 GUI/CLI 共用后端通过 Unix domain socket 通信，套接字位于 `/tmp/rtt2-<uid>/`。配置、项目历史和默认日志位于用户目录：Windows 为 `%LOCALAPPDATA%\RTT2\`，macOS 为 `~/Library/Application Support/RTT2/`，Linux 配置在 `$XDG_CONFIG_HOME/RTT2/`（默认 `~/.config/RTT2/`）、数据在 `$XDG_DATA_HOME/RTT2/`（默认 `~/.local/share/RTT2/`）。

协议字段、CRC 和兼容性见 [协议文档](docs/PROTOCOL.md)。发布版本记录见 [CHANGELOG.md](CHANGELOG.md)，参与开发见 [CONTRIBUTING.md](CONTRIBUTING.md)。本仓库目前**未附带开源许可证**。
