# 参与开发

RTT2 目前面向 Windows 和 ARTINX Kit-C、Kit-miao。提交问题时请写明板卡、芯片、调试探针、固件协议版本、复现步骤以及 status --json 的输出；分享日志前请检查其中的本机路径和设备信息。

提交代码前在仓库根目录运行：

~~~powershell
cargo fmt --all -- --check
cargo test --locked
~~~

涉及 RTT 协议的修改，请同时更新 docs/PROTOCOL.md 并添加对应的解析或黄金帧测试。涉及实板行为的结论，请区分单元测试、固件编译和实际板卡测试。
