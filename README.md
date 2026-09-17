# statusline-rs

Claude Code 状态栏的 Rust 实现：从 stdin 读取会话 JSON，向 stdout 输出两行状态文本。

替代原 bash + jq/awk 版本。后者每次刷新要 fork 七个进程，本机实测 562ms，而状态栏每 5 秒刷新一次；改成单进程后降到 16ms。

## 显示效果

```text
🤖 Opus 5 📁 statusline-rs/master 📂 agentscope-java
CTX 125k/1M +156/-23 📡 2 MCP 🪝 2 hooks ⚡ 3 skills
```

- 第一行：模型 | 当前目录末级 + git 分支/tag | `/add-dir` 附加的目录
- 第二行：上下文占用 | 本次会话增删行数 | 扩展配置计数

上下文占用超过窗口一半时，`CTX` 段后追加 `⚠`。

## 输入字段

Claude Code 每次渲染时经 stdin 传入一个 JSON 对象，程序消费其中这些字段：

| 字段 | 用途 | 缺失或异常时 |
| --- | --- | --- |
| `model.display_name` | 模型名 | 显示 `?` |
| `workspace.current_dir`（回退 `cwd`） | 取末级目录名；同时作为 git 探测起点 | 显示 `?` |
| `workspace.added_dirs` | 附加目录，逐项取末级后逗号连接 | 整段不显示 |
| `context_window.used_percentage` | 上下文占用百分比 | 视为 0 |
| `context_window.context_window_size` | 窗口大小，据此把占用换算成 `已用/总量` | 退回只显示百分比 |
| `cost.total_lines_added` / `total_lines_removed` | 本次会话增删行数 | 两者都非正数时整段不显示 |

已用 token 由「百分比 × 窗口大小」推算而非直接读字段，两者口径必然自洽。

输入为空则什么都不输出；JSON 非法则按空对象处理，状态栏照常渲染、各字段走默认值。

**尚未使用的字段**（都在 JSON 里，留作扩展点）：`context_window.remaining_percentage`、`context_window.current_usage`（token 明细）、`vim.mode`、`worktree.*`、`agent.name`、`pr.*`、`effort.level`、`thinking.enabled`、`fast_mode`、`session_name`、`workspace.git_worktree`、`workspace.repo`。

`rate_limits` 与 `cost.total_cost_usd` 在第三方 API（Anthropic 兼容端点）下不可靠，因此不显示；而增删行数是本地数工具调用 diff 得到的，与所用 provider 无关。

## 从文件系统读取的内容

除 stdin 之外，程序还会直接读文件，不启动任何子进程：

- **git 分支 / tag**：从 cwd 逐级向上找 `.git`，读其 `HEAD`。在分支上时 HEAD 是符号引用，取分支名；detached 时找指向该 commit 的 tag（附注 tag 读对象库解引用：松散对象、pack 内完整对象、packed-refs 的 `^` 解引发行都覆盖），找不到退回 7 位短 SHA。worktree 与 submodule 的 `.git` 是文件、内容形如 `gitdir: <路径>`，会顺指针再读；worktree 的引用与对象在 `commondir` 指向的共享 gitdir 里找。
- **扩展配置计数**：全局 `.claude.json`、全局 `settings.json`、项目 `.claude/`、项目根 `.mcp.json`，以及含 `SKILL.md` 的 skills 目录。`CLAUDE_CONFIG_DIR` 会被尊重。

## 安装

需要 Rust stable 工具链（本机为 `x86_64-pc-windows-gnu`）：

```bash
cargo build --release
cp target/release/statusline-rs.exe ~/.claude/statusline.exe
```

在 `~/.claude/settings.json` 中把状态栏指向它：

```json
"statusLine": {
  "type": "command",
  "command": "~/.claude/statusline.exe",
  "refreshInterval": 5
}
```

路径用正斜杠或 `~` 简写，**不要用反斜杠**：Windows 上该命令由 Git Bash 执行（等效 `bash.exe -c "<command>"`），未加引号的反斜杠会被它当转义符吃掉，`~\.claude\x.exe` 会变成 `~.claudex.exe`。用 `~` 而不是绝对路径，是为了让这份配置换机器也不用改。

## 设计说明

- **零子进程**：原 bash 版每渲染一次要 fork `cat`、多个 `grep`/`sed`/`awk`、`git` 共七个进程；Rust 版一个进程做完，git 信息全部改为读文件。
- **性能**：同机同口径实测 200 次平均，bash + jq 版 562ms/次，本版 16ms/次（约 35 倍）。其中约 13ms 是 Git Bash 创建进程的固定开销，程序自身净耗时约 3ms——省下的是那六次额外的进程创建。
- **行为对齐**：输出与原脚本逐字节比对过，空输入、缺字段等边界情况保持一致。
- **已知局限**：tag 只做精确匹配，detached 在无 tag 的提交上比 `git describe --tags --always` 少一个「最近 tag + 距离」的回溯；delta 存储的 tag 对象与 idx v1 包不解析，退回 7 位短 SHA（实测多个真实仓库的 tag 对象均以完整对象存储）。
- **体积**：release 配置启用 `strip` / LTO / `codegen-units=1` / `panic="abort"`，产物 356KB。
- **跨平台**：无平台特定逻辑，纯 std + serde_json，不启动任何子进程。
- **唯一依赖**：[serde_json](https://crates.io/crates/serde_json)。

## 项目结构

```text
src/main.rs     # 全部逻辑
src/inflate.rs  # 极简 zlib 解压（读 git 对象用），含单元测试
Cargo.toml      # 含 release 优化配置
```
