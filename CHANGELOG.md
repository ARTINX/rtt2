# 变更记录

## v1.1.0 — 2026-09-30

- 新增 GUI 命令 Shell 和 send、status CLI，通过本地命名管道共用 GUI 的探针连接。
- 新增 RTT down 0 一次性命令和 up 1 执行回执；当前支持 PING/ECHO。
- 保留 5 ms 遥测，显示 32 位 status、13 个浮点变量、时间戳和 CRC/丢帧统计。
- 曲线窗口可多开，共用主窗口的数据源；支持 CSV 记录、回放及单次发射记录。

## v1.0.0

- 初始 Windows RTT 遥测监视器。
