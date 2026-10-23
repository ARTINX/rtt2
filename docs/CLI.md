# RTT2 v1.4.0 CLI 使用说明

CLI 是常驻终端。启动一次，通过标准输入连续发送命令，直到 `quit`。它可以独立连接开发板，无需打开 GUI。以下用 `rtt2` 表示可执行文件（Windows 为 `rtt2.exe`）。

## 启动

| 操作 | 结果 |
| --- | --- |
| Windows 双击 EXE | GUI |
| CMD／PowerShell 中运行 `rtt2.exe` | 常驻 CLI，显示 `rtt2>` |
| 终端中运行 `./rtt2`（Linux/macOS） | 常驻 CLI，显示 `rtt2>` |
| `rtt2 cli` | 明确进入 CLI |
| `rtt2 cli --json` | 常驻 CLI，输出 JSONL，无提示符 |
| `rtt2 gui` | 明确打开 GUI |
| `rtt2 --version` / `--help` | 版本 / 启动帮助 |

Windows 在 PowerShell 当前目录下使用 `./rtt2.exe`；Linux 和 macOS 使用 `./rtt2`。把可执行文件放到 PATH 后可直接写 `rtt2`。输入重定向（包括 AI 或脚本的管道输入）自动输出 JSONL；非标准启动器可显式指定 `cli` 或 `gui`。macOS/Linux 从桌面直接启动且没有终端时，会打开 GUI。

使用 `cargo build --release --locked --no-default-features` 构建的 CLI-only 版本始终进入命令行，`rtt2 gui` 和 CLI 内的 `gui` 命令会提示未包含 GUI。

## 终端编辑与历史

终端模式下（标准输入和标准输出都连着终端，且未加 `--json`）输入行支持 readline 式编辑：

| 按键 | 作用 |
| --- | --- |
| ←/→、Ctrl+B/Ctrl+F | 左右移动光标 |
| Home/End、Ctrl+A/Ctrl+E | 行首/行尾 |
| Ctrl+←/→、Alt+B/Alt+F | 按词移动 |
| Backspace/Delete、Ctrl+W | 删除字符/前一个词 |
| Ctrl+U | 删除整行 |
| ↑/↓ | 翻阅历史；Ctrl+R 反向增量搜索 |
| Tab | 补全命令、`connect` 选项和芯片型号 |
| Ctrl+C | 放弃当前输入行（不退出终端） |
| Ctrl+D | 输入结束时退出终端 |

历史保存在数据目录的 `cli_history.txt`（Windows `%LOCALAPPDATA%\RTT2\`，macOS `~/Library/Application Support/RTT2/`，Linux `$XDG_DATA_HOME/RTT2/`，默认 `~/.local/share/RTT2/`），最多 2000 条，跨终端保留。

`catch` 完成或中断时，即使正在输入，结果也会打印在提示符上方并重绘当前行。

## 连接与发送

```text
connect --project "D:\firmware\bsp\cubemx\MDK-ARM\kit-miao" --chip STM32H723VG
send 0
send 1 1.0 2.0
status
```

直接用 ELF 符号地址连接（适合 ELF 已删除或被 strip 的场合）：

```text
connect --address 0x20009fa4 --chip STM32H723VG
```

Linux/macOS 示例：

```text
connect --project "/home/me/firmware/bsp/cubemx/MDK-ARM/kit-miao" --chip STM32H723VG
connect --project "/home/me/firmware/build/firmware.elf" --chip STM32H723VG
```

Kit-C 芯片为 `STM32F407IGHx`。路径可写 `~`、`~/目录` 或绝对/相对路径：`~` 展开为当前用户主目录，相对路径以终端当前目录为基准。路径中有空格时加双引号或单引号；Windows 反斜杠保持原样。`--project` 可指向板卡输出目录或单个固件文件；目录会被递归搜索 `.elf`、`.axf`、`.out`（最多 8 层，跳过 `.git`、`target`、`node_modules`、`.venv`），并读取与已烧录固件匹配的 `_SEGGER_RTT` 符号。Keil `.axf`、armgcc/armclang `.elf`、IAR `.out` 都是 ELF，读取方式相同；`.map` 不再作为地址来源。存在多个不同 RTT 地址时拒绝连接。

`--address` 直接指定 RTT 控制块地址（十六进制用 `0x` 前缀，`nm`/`readelf` 输出的裸十六进制也可），优先于 `--project`；指定地址后会清掉保存的工程目录，之后裸 `connect` 继续使用该地址。省略两者并只给 `--chip` 时，由后端扫描 RAM 定位控制块。

成功连接后配置保存到按平台划分的目录，下次直接 `connect` 即可：Windows 为 `%LOCALAPPDATA%\RTT2\connection.json`，macOS 为 `~/Library/Application Support/RTT2/connection.json`，Linux 为 `$XDG_CONFIG_HOME/RTT2/connection.json`（默认 `~/.config/RTT2/`）。保存的工程目录在每次 `connect` 时重新解析，因此重新编译后会使用新的控制块地址。更换固件后重新编译并指定匹配的目录。连接过程不会烧录或复位开发板。

`send` 接受 0–255 的命令号和最多 12 个有限 float32。现有固件提供 PING（0，无参数）与 ECHO（1，回显参数），其他业务指令取决于固件实现。回执含执行状态、会话 ID、序号、板端时间、返回值和主机测得延迟。`applied` 表示板端已确认执行；`unknown` 表示执行结果未知，不能据此自动重发。回执超时保持 2 秒；本地 IPC 另有超时保护。

`status` 包含 connected、connecting、command_available、config、samples_received、crc_errors、lost_frames、latest_sample、last_error、capture 和 last_capture。计数从本次板卡连接开始，latest_sample 含 seq、timestamp_ms、status 和按 value1–value13 排列的 values。JSON 中正常浮点值为数字，非有限遥测值明确编码为字符串 `NaN`、`Infinity`、`-Infinity`，不会伪装成有效数值。

## 定时采集

```text
catch 10 "trial one.csv"
send 0
wait
```

`catch <duration> <file_name>` 的时长单位为秒，支持 `10`、`0.5`、`10s`；范围大于 0 且不超过 86400 秒。文件按 CSV 写入，扩展名由用户决定。相对路径以终端当前目录为基准，也支持 `~/文件名.csv`；父目录必须存在，已有文件一律拒绝覆盖。

采集从命令被后端接受后开始，按主机收到数据的单调时间计时。它记录此后收到的样本，不导出之前的曲线历史；板端 timestamp_ms 保留原值。实际帧率和丢帧取决于固件、探针与主机，不保证每 5 ms 都收到一帧。

采集在后台运行，命令行立即返回，可继续 `send`、`status`。同一共享会话一次只允许一个定时采集。`wait` 等待当前采集完成；`cancel` 取消本终端发起的采集并保留已写入部分。

CSV 列为 `seq,timestamp_ms,status,status_hex,value1,...,value13`。采集结束输出文件绝对路径、请求/实际时长、帧数、采样序号间隙数、序号重置数、CRC 错误增量和完成状态。CRC 计数由后端周期统计更新，因此短采集的 CRC 增量不是逐字节精确审计。丢帧数按采集窗口内相邻有效帧序号统计，不包含第一帧之前或最后一帧之后的丢失。

完成状态：`completed` 正常到时，`interrupted` 提前结束，`error` 写入失败。断连、`quit`、输入结束和 Ctrl+C 会保存部分文件；需要完整采集时先 `wait` 再 `quit`。终端提示符处的 Ctrl+C 只放弃当前输入行，不会退出，也不会中断采集；要提前结束终端请用 Ctrl+D 或 `quit`。

## CLI 与 GUI

GUI、CLI 是公共后端的两个前端。后端独占探针并分发遥测、命令回执；CLI 的采集由后端执行，不依赖窗口刷新。

- 已连接时，在 CLI 输入 `gui` 或启动同版本程序，GUI 自动接入现有连接。
- GUI 已连接时进入 CLI，可直接 `status`、`send`、`catch`。
- 关闭 GUI 或 `quit` 只退出该前端；其他前端继续使用连接。本终端发起的未完成定时采集会在退出时结束。
- `disconnect`／GUI 的“停止监控”是明确的全局断开操作，会影响所有前端并结束采集。
- 最后一个前端退出后，后端约 3 秒后释放连接并退出。异常结束的前端最多约 30 秒后被回收。
- Windows 后端通过命名管道通信；Linux/macOS 通过 `/tmp/rtt2-<uid>/rtt2-session-v130-<uid>.sock` 通信，目录和套接字仅当前用户可访问。残留套接字会在下次启动时自动清理。
- 请先关闭旧版 RTT2；v1.2 的 GUI 不参与 v1.3 的连接共享。

GUI 保留曲线、回放、连续 CSV 和单发记录功能。关闭 GUI 时其自身连续记录会结束；CLI 定时采集由发起 CLI 的生命周期控制。

## AI／脚本

使用一个持续存活的进程，通过 stdin 写入命令并立即刷新，通过 stdout 按行读取 JSON。不要为每条命令重新启动程序。

```python
import json
import subprocess

p = subprocess.Popen(
    ["rtt2", "cli", "--json"],  # Windows 可写 r"D:\tools\rtt2.exe"
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    text=True, encoding="utf-8", bufsize=1,
)

def command(line, expected_event):
    p.stdin.write(line + "\n")
    p.stdin.flush()
    while True:
        raw = p.stdout.readline()
        if not raw:
            raise RuntimeError("RTT2 exited: " + p.stderr.read())
        result = json.loads(raw)
        if result["event"] == "error":
            raise RuntimeError(result["data"]["error"])
        if not result["ok"]:
            raise RuntimeError(result)
        if result["event"] == "capture_finished":
            print(result["data"])
        if result["event"] == expected_event:
            return result["data"]

try:
    print(json.loads(p.stdout.readline()))  # ready
    command(r'connect --project "/home/me/firmware/kit-miao" --chip STM32H723VG', "connect")
    command("catch 10 trial.csv", "catch")
    command("send 0", "send")
    command("wait", "wait")  # 期间还会收到 capture_finished
finally:
    if p.poll() is None:
        p.stdin.write("quit\n")
        p.stdin.flush()
        p.communicate(timeout=20)
```

每行结构为 `{"event":"事件名","ok":true,"data":{...}}`。事件包括 ready、connect、send、catch、capture_finished、status、wait、cancel、disconnect、gui、help、error、quit。后台的 capture_finished 可出现在任何命令间隙；脚本应持续消费输出并检查其 ok 和完成状态。首次启动失败等进程级错误写到 stderr，并返回非零退出码。

退出码：0 本次终端无失败；1 启动或不可恢复错误；2 参数/操作失败或采集中断；3 命令结果未知；4 命令队列满；5 命令连接不可用。终端会累计失败状态，后续成功或 quit 不会抹去此前失败。命令被拒绝也返回非零。

v1.2 的 `rtt2 send ...`、`rtt2 status --json` 不再提供。请将它们改为常驻 CLI 内的 `send`、`status`。
