# 参与开发

RTT2 面向 Windows、Linux、macOS 和 ARTINX Kit-C、Kit-miao。提交问题时请写明操作系统、板卡、芯片、调试探针、固件协议版本、复现步骤以及 `rtt2 cli --json` 中 `status` 的输出；分享日志前请检查其中的本机路径和设备信息。

提交代码前在仓库根目录运行：

~~~sh
cargo fmt --all -- --check
cargo test --locked
cargo test --locked --no-default-features
~~~

Windows、Linux 和 macOS 都受支持。Linux 上访问探针通常需要 probe-rs udev 规则或相应用户组权限；涉及平台相关的进程、路径或 IPC 修改时，请同时在 Unix 和 Windows 上验证。

涉及 RTT 协议的修改，请同时更新 docs/PROTOCOL.md 并添加对应的解析或黄金帧测试。涉及实板行为的结论，请区分单元测试、固件编译和实际板卡测试。

CLI／GUI 共用连接的代码在 `src/session.rs`，本地通信在 `src/local_pipe.rs`，命令解析在 `src/cli.rs`，跨平台用户目录在 `src/paths.rs`。修改采集或生命周期时，需覆盖截止时间、部分文件保存、不可覆盖和前端退出互不影响。
