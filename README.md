# RTT2

RTT2 是面向 ARTINX Kit-C 和 Kit-miao 的 Windows 桌面调试工具。它通过 probe-rs 连接 CMSIS-DAP 或 J-Link 探针，从 Keil .map 文件定位 SEGGER RTT 控制块，显示 5 ms 遥测，并通过 RTT 发送一次性命令。界面和命令行共用同一个探针连接。

## 功能

- 实时显示 debug_info.status（32 位二进制、十六进制、十进制）和 v2 至 v14 的数值。
- 多个独立曲线窗口共用主窗口接收的数据；支持 CSV 记录、回放和单次发射记录。
- GUI 命令 Shell 与 send CLI 发送一次性 PING/ECHO 命令，按会话和序号匹配执行回执。
- 遥测含序号、板端时间戳和 CRC16；坏帧与丢帧分别统计。

RTT2 目前仅支持 Windows。固件在独立的 ARTINX 工程中维护；本仓库只包含上位机代码。使用前请编译并烧录支持 RTT2 协议的固件。

## 快速开始

1. 从 v1.1 Release 下载 rtt2v1.1.exe，连接开发板和调试探针。
2. 双击程序，选择芯片与**该板卡的 Keil 编译输出目录**。目录中需有包含 _SEGGER_RTT 符号的 .map 文件。
3. 点击“启动 RTT 监控”。主窗口右侧显示状态和原始变量；“新建曲线窗口”可同时打开多个曲线窗口。
4. 根据需要开始 CSV 记录、加载 CSV 回放，或点击“记录本发”保存一次发射时的快照。

| 板卡 | 芯片型号 | 工程输出目录示例 |
| --- | --- | --- |
| Kit-C | STM32F407IGHx | artinx-2026-frame/bsp/cubemx/MDK-ARM/Kit-C |
| Kit-miao | STM32H723VG | artinx-2026-frame/bsp/cubemx/MDK-ARM/kit-miao |

程序会拒绝包含多个不同 RTT 地址的目录；请选具体板卡的输出目录。若连接到 RTT 控制块后 5 秒内没有有效遥测，程序会断开并提示检查固件或复位开发板。

### 命令 Shell 与 CLI

GUI 左侧的命令 Shell 接受 send <cmd_id> [float 参数…]。按一次 Enter 或点击一次发送按钮只提交一次命令。当前固件实现 send 0（无参数 PING）和 send 1 1.0 2.0（ECHO，最多 12 个有限 float32）。其他命令号返回 UNKNOWN_CMD。

GUI 必须先连接开发板；CLI 通过当前 Windows 用户的本地命名管道交给 GUI，自己不会连接探针：

~~~powershell
$exe = '.\rtt2v1.1.exe'
& $exe status --json
& $exe send 0 --json
& $exe send 1 1.0 2.0 --json
~~~

也可从脚本启动并连接 GUI：

~~~powershell
$project = 'C:\path\to\artinx-2026-frame\bsp\cubemx\MDK-ARM\kit-miao'
Start-Process -FilePath '.\rtt2v1.1.exe' -ArgumentList @('gui', '--project', $project, '--chip', 'STM32H723VG', '--connect')
~~~

send --json 返回执行状态、会话 ID、序号、板端时间戳、回显值和主机测得的回执延迟。退出码：0 已执行，2 被拒绝或参数无效，3 结果未知，4 GUI 本地请求队列已满，5 GUI 未运行或未连接。命令回执默认等待 2 秒；超时不自动重试，因为板端可能已经执行。当前固件保持原有 Message_Publish 接口，内部消息队列投递失败时无法立即产生 QUEUE_FULL 回执，上位机会报告“结果未知”。

status --json 只读取 GUI 状态，不发送命令；它提供连接状态、遥测帧数、CRC/丢帧计数和累计探针 I/O 时间。

## 从源码构建

安装支持 Rust 2024 edition 的 Rust 工具链和 Windows C++ 构建工具，在仓库根目录运行：

~~~powershell
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
~~~

产物是 target/release/rtt2.exe。可运行 cargo run --example stability -- 'C:\path\to\Kit-C' 30 STM32F407IGHx 做实板稳定性观察。界面刷新间隔默认 5 ms；改变它不会改变固件采样周期。

协议字段、CRC 和兼容性见 [协议文档](docs/PROTOCOL.md)。发布版本记录见 [CHANGELOG.md](CHANGELOG.md)，参与开发见 [CONTRIBUTING.md](CONTRIBUTING.md)。本仓库目前**未附带开源许可证**。
