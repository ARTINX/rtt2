# v1.3.0 验证记录

日期：2026-10-23；平台：Windows x64。

## 已验证

- `cargo test --locked --offline`：31 项通过，包含 24 项库测试、5 项 GUI 逻辑/渲染测试和 2 项真实进程集成测试。
- `cargo fmt --all -- --check` 和 `git diff --check`。
- `cargo clippy --locked --offline --lib --bin rtt2 -- -D warnings`：正式库与程序通过。
- `cargo build --release --locked --offline`：优化构建通过。
- 真实 Windows 命名管道：单后端实例约束、超过管道缓冲区的 Unicode 消息双向传输；空读等待和分块写入。
- 真实终端进程：无参数且输入重定向时进入 CLI；多个 CLI 共用后端，退出其中一个后其余可继续 status；stdin EOF、quit、错误返回码、版本和帮助。
- 最终 Release EXE：版本检查、JSONL 的 ready/status/quit 往返通过；实际终端无参数启动显示 `rtt2>`，status、quit 正常。
- 原生窗口：无参数桌面启动打开 GUI，标题为 RTT2 v1.3.0 实时监控，主界面正常显示。
- 内存中的模拟遥测/回执：采集期间继续收样与处理一次命令回执；GUI 订阅事件与采集互不影响；退出一个前端保留其余前端的连接。
- CSV：17 列完整数据、禁止覆盖、无效时长不创建文件、截止后样本排除、断连保留部分文件、序号间隙统计。
- 非有限遥测：NaN、正负无穷经 JSON IPC 后仍明确保留，CSV 中不会替换成正常数值。

## 证据边界

本次未执行 Kit-C / Kit-miao 的实板连接、PING/ECHO、200 Hz 连续采集或实板 GUI/CLI 并用测试。上述模拟测试不证明探针吞吐、实板时序或执行效果。固件协议与 2 秒回执超时保持不变；现场使用前仍需匹配已烧录固件的 MAP 输出目录。

编译器会输出 MSVC 链接器创建库文件的提示；这不影响构建成功。测试代码中的既有 Clippy 风格提示不作为本次正式程序的静态检查范围。
