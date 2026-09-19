//! Claude Code 状态栏：从 stdin 读取会话 JSON，渲染模型、当日 token 用量、
//! 目录、Git 分支、上下文占用与扩展配置计数。
//!
//! 数据有两个来源：stdin 传入的会话 JSON，以及 `~/.claude/projects` 下的会话
//! 日志。后者是当日 token 统计所必需的——Claude Code 不通过 stdin 传累计值。
//!
//! 替代原 bash + jq/grep/awk 实现。原版每次刷新要 fork 七个进程（Windows 上约
//! 0.6 秒），而状态栏每 5 秒刷新一次；这里单进程完成，不产生任何子进程。

mod inflate;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// 本机时区相对 UTC 的偏移秒数。
///
/// Rust 标准库不提供本地时区能力，而「当日」是本地日历概念：会话日志里的
/// timestamp 是 UTC，要按本地日界切分就必须有这个偏移。这里直接写死 UTC+8，
/// **换到别的时区使用必须改这个值**，否则日界会整体偏移、凌晨的用量会算错。
const LOCAL_OFFSET_SECONDS: i64 = 8 * 3600;

const SECS_PER_DAY: i64 = 86400;

const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const WHITE: &str = "\x1b[37m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

fn main() {
    let mut buf = Vec::new();
    if std::io::stdin().read_to_end(&mut buf).is_err() || buf.is_empty() {
        return;
    }
    // 解析失败按空对象处理，各字段走默认值，状态栏仍能渲染
    let root: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);

    let _ = std::io::stdout().write_all(render(&root).as_bytes());
}

fn render(root: &Value) -> String {
    let model = at(root, &["model", "display_name"])
        .and_then(Value::as_str)
        .unwrap_or("?");
    let cwd = at(root, &["workspace", "current_dir"])
        .or_else(|| at(root, &["cwd"]))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let short_dir = last_component(cwd);
    let branch = git_branch(Path::new(cwd))
        .map(|b| format!("/{b}"))
        .unwrap_or_default();

    // 非有限值与非正数一并归零，后面所有分支共用这个已归一的值
    let pct = at(root, &["context_window", "used_percentage"])
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(0.0);
    // 首选 token 明细，已用量由百分比与窗口大小推算，两者口径必然自洽；
    // 拿不到窗口大小时退回百分比
    let window_size = at(root, &["context_window", "context_window_size"])
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let ctx_text = if window_size > 0.0 {
        let used = (window_size * pct / 100.0) as u64;
        format!("{}/{}", fmt_k(used), fmt_k(window_size as u64))
    } else {
        format!("{}%", pct as i64)
    };
    // 占用超过窗口一半时警示。不用 JSON 里的 `exceeds_200k_tokens`：那个阈值是
    // 按 Claude 标准的 200k 窗口定的，对 1M 窗口的模型只相当于两成占用，会误报。
    // ⚠ 是宽字符，尾部多带一个空格（加上 join 的分隔符共两个），免得和后面的
    // 增删行数贴在一起
    let over_mark = if pct >= 50.0 { " ⚠ " } else { "" };

    let dirs_display = match added_dirs(root) {
        Some(dirs) => format!("{BOLD}{YELLOW} 📂 {dirs}{RESET}"),
        None => String::new(),
    };
    // 当日用量放在第一行：它与模型、目录同属「这会儿在哪个项目、花了多少」
    // 这一组信息，而第二行留给上下文占用与扩展配置。没有数据时整段不出现
    let usage_display = today_usage()
        .map(|usage| {
            let mut text = format!("🔤 今日 {}", fmt_k(usage.total()));
            if let Some(permille) = usage.cache_hit_permille() {
                text.push_str(&format!(" 缓存 {}", fmt_permille(permille)));
            }
            format!(" {WHITE}{text}{RESET}")
        })
        .unwrap_or_default();

    // 逐段拼装，缺数据的段直接不参与，避免留下分隔空格
    let mut segments = vec![format!("{WHITE}CTX {ctx_text}{RESET}{over_mark}")];
    if let Some(lines) = line_changes(root) {
        segments.push(lines);
    }
    let (mcp, hooks, skills) = counts(cwd);
    let mut summary = Vec::new();
    if mcp > 0 {
        summary.push(format!("📡 {mcp} MCP"));
    }
    if hooks > 0 {
        summary.push(format!("🪝 {hooks} hooks"));
    }
    if skills > 0 {
        summary.push(format!("⚡ {skills} skills"));
    }
    if !summary.is_empty() {
        segments.push(format!("{WHITE}{}{RESET}", summary.join(" ")));
    }
    let mut out = String::new();
    out.push_str(&format!(
        "🤖 {BOLD}{CYAN}{model}{RESET}{usage_display} 📁 {WHITE}{short_dir}{branch}{RESET}{dirs_display}\n"
    ));
    out.push_str(&segments.join(" "));
    out.push('\n');
    out
}

/// 本会话累计增删行数。这是 Claude Code 本地数工具调用 diff 得到的，与所用
/// provider 无关；增减都为零时视为无数据，不显示
fn line_changes(root: &Value) -> Option<String> {
    let added = at(root, &["cost", "total_lines_added"])
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as i64;
    let removed = at(root, &["cost", "total_lines_removed"])
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as i64;
    if added <= 0 && removed <= 0 {
        return None;
    }
    Some(format!("{GREEN}+{added}{RESET}/{RED}-{removed}{RESET}"))
}

/// token 数缩写：200000 → 200k，1000000 → 1M，1500000 → 1.5M
fn fmt_k(n: u64) -> String {
    if n >= 1_000_000 {
        let millions = n as f64 / 1_000_000.0;
        // 零头不足显示精度（0.1M）时省掉小数点，免得把 1M 写成 1.0M
        if (millions - millions.round()).abs() < 0.05 {
            format!("{}M", millions.round() as u64)
        } else {
            format!("{millions:.1}M")
        }
    } else if n >= 1000 {
        format!("{}k", (n + 500) / 1000)
    } else {
        n.to_string()
    }
}

/// 千分数转一位小数的百分比文本：986 → `98.6%`。
///
/// 与 `fmt_k` 并列，让「数值 → 显示文本」保持单一入口：整数除法截断的逻辑
/// 收在这里，调用方不必知道自己手上拿到的是千分数
fn fmt_permille(permille: u64) -> String {
    format!("{}.{}%", permille / 10, permille % 10)
}

/// 按路径取字段，任一层缺失都返回 None。全文件统一用它取值
fn at<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(root, |cur, key| cur.get(key))
}

/// 取路径最后一段。用 `Path` 而非手写切分，尾部分隔符（`D:/proj/`）也能正确取到
/// 目录名，而字符串切分会得到空串
fn last_component(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// `/add-dir` 添加的目录，逐项取末级目录名后以逗号连接。无目录时返回 None，
/// 由调用方决定整段是否出现
fn added_dirs(root: &Value) -> Option<String> {
    let names: Vec<&str> = at(root, &["workspace", "added_dirs"])
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(last_component)
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

/// 全局 Claude 配置目录、以及 `.claude.json` 的路径。
///
/// 未设 `CLAUDE_CONFIG_DIR` 时全局配置在 home 下，而非 home/.claude 下。
fn claude_dirs() -> (PathBuf, PathBuf) {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            let json = dir.join(".claude.json");
            (dir, json)
        }
        None => {
            let home = std::env::home_dir().unwrap_or_default();
            (home.join(".claude"), home.join(".claude.json"))
        }
    }
}

/// 统计 MCP server、hook 事件、skill 数量。
///
/// 配置分四层：全局 `.claude.json`、全局 `settings.json`、项目 `.claude/`、
/// 项目根的 `.mcp.json`。cwd 恰好是配置文件目录本身时跳过项目层，否则会与全局
/// 层重复读取同一文件。
fn counts(cwd: &str) -> (usize, usize, usize) {
    let (claude_dir, global_json) = claude_dirs();

    // 每个文件只读一次、只解析一次，两个键共用同一份文档
    let global = load(&global_json);
    let user_settings = load(&claude_dir.join("settings.json"));
    let mut mcp =
        count_in(global.as_ref(), "mcpServers") + count_in(user_settings.as_ref(), "mcpServers");
    let mut hooks = count_in(global.as_ref(), "hooks") + count_in(user_settings.as_ref(), "hooks");

    let proj_dir = Path::new(cwd).join(".claude");
    let distinct = matches!(
        (
            std::fs::canonicalize(&proj_dir),
            std::fs::canonicalize(&claude_dir)
        ),
        (Ok(p), Ok(c)) if p != c
    );

    if distinct {
        let project = load(&proj_dir.join("settings.json"));
        let project_local = load(&proj_dir.join("settings.local.json"));
        hooks += count_in(project.as_ref(), "hooks") + count_in(project_local.as_ref(), "hooks");
    }
    let project_mcp = load(&Path::new(cwd).join(".mcp.json"));
    mcp += count_in(project_mcp.as_ref(), "mcpServers");

    let mut skills = count_skills(&claude_dir.join("skills"));
    if distinct {
        skills += count_skills(&proj_dir.join("skills"));
    }

    (mcp, hooks, skills)
}

/// 会话日志里一条 assistant 记录中我们关心的字段。
///
/// 未列出的字段（尤其 `message.content` 里的正文与 thinking，占一行绝大部分
/// 字节）由 serde 跳过、不分配内存——这是本模块能每 5 秒全量重扫当日文件的
/// 前提。若换成解析成 `Value`，光是把这些正文读进内存就够呛。
#[derive(Deserialize)]
struct LogLine<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    timestamp: Option<&'a str>,
    message: Option<LogMessage<'a>>,
}

#[derive(Deserialize)]
struct LogMessage<'a> {
    id: Option<&'a str>,
    stop_reason: Option<&'a str>,
    usage: Option<Usage>,
}

/// 容器级 `#[serde(default)]`：四个字段都可能缺（不同 provider 上报的
/// 缓存字段不一致），缺哪个补哪个，不必逐字段标注
#[derive(Deserialize, Default, Clone, Copy)]
#[serde(default)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

impl Usage {
    /// 四项相加。口径与 CC Switch 使用统计页的「真实消耗 Tokens」一致
    fn total(self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }

    /// 缓存命中率 = 命中量 / 输入侧总量，与 CC Switch 同式。
    ///
    /// 分母只含输入三项、不含 output：output 永远不可能命中缓存，算进去
    /// 只会把命中率稀释。分母为 0（当天没有输入）时返回 None 而非 0，
    /// 由调用方决定不显示——报「0%」会把「无数据」说成「一次都没命中」。
    ///
    /// 返回**千分数**（如 986 表示 98.6%）而非浮点比率：整数除法天然截断，
    /// 避开 `{:.1}` 的四舍五入，也躲开 0.986 之类无法精确表示的浮点误差。
    fn cache_hit_permille(self) -> Option<u64> {
        let input_side =
            self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens;
        (input_side > 0).then(|| {
            // 先升到 u128 再乘，token 数再大也不会溢出
            (self.cache_read_input_tokens as u128 * 1000 / input_side as u128) as u64
        })
    }
}

/// 四个维度逐项累加。`Usage` 同时充当当日累加器——它的形状本来就与
/// 一条记录相同，另立一个类型只是重复。
impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

/// 同一 message 去重后留下的代表条目
struct Deduped {
    usage: Usage,
    has_stop: bool,
}

/// 当日（本地 0 点起）Claude Code 的 token 用量，四个维度分开返回。
///
/// 总量口径与 CC Switch 的使用统计一致：四项相加。两者同源——CC Switch 也读
/// `~/.claude/projects` 下的会话 JSONL——但它的会话同步是每 60 秒一轮，
/// 这里每次刷新重扫，所以数字更新鲜、无需 CC Switch 在运行。
///
/// 读失败一律当作 0 贡献而非报错：状态栏不该因为一个读不了的历史文件
/// 而整段消失。当天一条用量都没有时返回 None，由渲染层决定整段不显示。
fn today_usage() -> Option<Usage> {
    let projects = claude_dirs().0.join("projects");
    let day_start = local_day_start();

    // 去重表跨文件共用：message.id 是全局标识，CC Switch 侧也是靠
    // `session:<msg_id>` 主键在库层面跨文件去重。若同一个 id 出现在两个当日
    // 文件里（会话分叉、续接会把历史记录带进新文件），按文件各去各的会把它
    // 计两次，且表现为用量偏高、没有任何提示
    let mut by_id: HashMap<String, Deduped> = HashMap::new();
    for path in collect_session_files(&projects, day_start) {
        scan_file(&path, day_start, &mut by_id);
    }

    let mut usage = Usage::default();
    for entry in by_id.values() {
        // 只算真正产生过计费 token 的记录：Anthropic 在受理请求时就对 input
        // 与 cache 计费，Workflow/子 agent 的短命请求常只写了 message_start
        // 快照（output 为 0、无 stop_reason）却没写最终块，成本已实际发生
        if entry.usage.total() > 0 {
            usage += entry.usage;
        }
    }
    (usage.total() > 0).then_some(usage)
}

/// 收集今日写过的会话 JSONL，固定深度遍历（不递归，避免死循环）：
///
/// ```text
/// projects/<项目>/<会话>.jsonl                                    主会话
/// projects/<项目>/<会话ID>/subagents/*.jsonl                      子 agent
/// projects/<项目>/<会话ID>/subagents/workflows/wf_*/*.jsonl      Workflow 子 agent
/// ```
///
/// 后两层是 Task/Agent 与 Workflow 产生的记录。子 agent 用量占比可观，
/// 漏掉任何一层都会系统性少算。
///
/// 只收 mtime 落在今日的文件：今日产生的消耗必然让文件 mtime 变成今日，
/// 今日没被写过的文件必然不含今日记录。反过来也成立，两个方向都不会漏。
fn collect_session_files(projects: &Path, day_start: i64) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for project in subdirs(projects) {
        push_jsonl(&project, day_start, &mut files);
        for session in subdirs(&project) {
            let subagents = session.join("subagents");
            push_jsonl(&subagents, day_start, &mut files);
            for workflow in subdirs(&subagents.join("workflows")) {
                push_jsonl(&workflow, day_start, &mut files);
            }
        }
    }
    files
}

/// 目录下的一级条目路径。目录不存在或读不了都当空列表——调用方因此不必
/// 为每一层再写一遍存在性判断（那还会多花一次 stat）
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries.flatten().map(|entry| entry.path()).collect()
}

/// 把目录下直接持有、且写过今日的 `.jsonl` 收进列表，不进子目录。
///
/// 用 `DirEntry::metadata()` 而非事后对路径再 `fs::metadata`：Windows 上前者
/// 复用目录扫描已取回的数据、不额外发系统调用，而历史会话文件只增不减，
/// 逐个补 stat 的成本会随积累无上界。扩展名也先用 `file_name` 判，免得为
/// 目录里那些不是 `.jsonl` 的条目白分配一个完整 `PathBuf`。
fn push_jsonl(dir: &Path, day_start: i64, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if Path::new(&entry.file_name())
            .extension()
            .and_then(|e| e.to_str())
            != Some("jsonl")
        {
            continue;
        }
        let fresh = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(epoch_secs)
            .is_some_and(|mtime| mtime >= day_start);
        if fresh {
            files.push(entry.path());
        }
    }
}

/// 扫描单个会话文件，把其中属于当日的记录并入 `by_id`。
fn scan_file(path: &Path, day_start: i64, by_id: &mut HashMap<String, Deduped>) {
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    // 整份转 &str 后再扫：`&[u8]` 的 split/contains 走标量循环，整份数据逐字节
    // 扫下来要几毫秒，而 str 的对应实现用 memchr / TwoWay。但写入方可能正在
    // 追加，残段被截在多字节字符中间就会让整份数据失去 UTF-8 合法性——那种
    // 情况退回逐行按字节处理，宁可慢也不丢掉整个文件
    match std::str::from_utf8(&data) {
        Ok(text) => {
            for line in text.split('\n') {
                scan_line(line, day_start, by_id);
            }
        }
        Err(_) => {
            for line in data.split(|&b| b == b'\n') {
                if let Ok(line) = std::str::from_utf8(line) {
                    scan_line(line, day_start, by_id);
                }
            }
        }
    }
}

/// 处理一行。只认 assistant 记录，其余整行跳过。
fn scan_line(line: &str, day_start: i64, by_id: &mut HashMap<String, Deduped>) {
    // `"usage"` 是 assistant 记录的特征字段，用它预筛可让 user/progress 行
    // （占行数的大头）一行都不进 serde。这个字面量必须与 `LogLine` 的字段名
    // 保持同步：字段改了这里没改，会静默跳过全部行、当日用量直接变 0，
    // 而编译器和类型系统都不会提示
    if !line.contains("\"usage\"") {
        return;
    }
    let Ok(parsed) = serde_json::from_str::<LogLine>(line) else {
        return;
    };
    if parsed.kind != Some("assistant") {
        return;
    }
    let (Some(timestamp), Some(message)) = (parsed.timestamp, parsed.message) else {
        return;
    };
    // 跨夜会话文件里混着昨天的记录，按行时间戳再筛一次
    if parse_epoch(timestamp).is_none_or(|t| t < day_start) {
        return;
    }
    let (Some(id), Some(usage)) = (message.id, message.usage) else {
        return;
    };
    keep_representative(by_id, id, usage, message.stop_reason.is_some());
}

/// 同一 message.id 在流式响应中会按 block 多次落盘（本机实测平均约 3 条），
/// 不去重会把用量放大数倍。这里留下代表条目，选法与 CC Switch 一致：优先留
/// 有 stop_reason 的（最终态，token 已定），同为终态或同为中间态时取 output
/// 更大的那条。
fn keep_representative(
    by_id: &mut HashMap<String, Deduped>,
    id: &str,
    usage: Usage,
    has_stop: bool,
) {
    // 先用 &str 探一次：已存在的 key 占多数，这样不必为它们白构造 String
    let Some(existing) = by_id.get_mut(id) else {
        by_id.insert(id.to_string(), Deduped { usage, has_stop });
        return;
    };
    // 新建时不会走到这里，所以下面的判定只需处理「已存在」这一种情形
    let replace = if has_stop == existing.has_stop {
        usage.output_tokens > existing.usage.output_tokens
    } else {
        has_stop
    };
    if replace {
        *existing = Deduped { usage, has_stop };
    }
}

/// `SystemTime` 转 Unix 秒
fn epoch_secs(time: std::time::SystemTime) -> Option<i64> {
    Some(time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// 本地今日 0 点对应的 Unix 秒
fn local_day_start() -> i64 {
    // 取不到系统时间（时钟早于 epoch）时按 0 起算：日界落到 1970 年，结果是
    // 全部记录都被当作今日，比整段消失更接近用户的预期
    let now = epoch_secs(std::time::SystemTime::now()).unwrap_or(0);
    let local = now + LOCAL_OFFSET_SECONDS;
    local - local.rem_euclid(SECS_PER_DAY) - LOCAL_OFFSET_SECONDS
}

/// RFC3339 UTC 时间戳转 Unix 秒，形如 `2026-09-19T14:35:17.795Z`。
///
/// 只认 Claude Code 实际写出的定长 UTC 格式，按位取数；带时区偏移或格式
/// 变了都返回 None（该条不计入）。不猜：把 `+08:00` 的时间戳当 UTC 算，
/// 会让日界判定整体偏移 8 小时。
fn parse_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    // 第 20 位必须是 `.` 或 `Z`——正是这一位挡掉带 `+08:00` 偏移的写法
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || (b[19] != b'.' && b[19] != b'Z')
    {
        return None;
    }
    let (year, month, day) = (digits(b, 0, 4)?, digits(b, 5, 2)?, digits(b, 8, 2)?);
    let (hour, minute, second) = (digits(b, 11, 2)?, digits(b, 14, 2)?, digits(b, 17, 2)?);
    Some(days_from_civil(year, month, day) * SECS_PER_DAY + hour * 3600 + minute * 60 + second)
}

/// 取 `from` 起 `len` 位的十进制数字；越界或含非数字返回 None
fn digits(b: &[u8], from: usize, len: usize) -> Option<i64> {
    let mut value = 0i64;
    for &c in b.get(from..from + len)? {
        if !c.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(c - b'0');
    }
    Some(value)
}

/// 公历日期转「自 1970-01-01 起的天数」。
/// 出处：Howard Hinnant 的 days_from_civil，先把 3 月当作年首以绕开闰日。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 读取并解析配置文件；文件缺失或不是合法 JSON 都返回 None，对应「该项计 0」
fn load(path: &Path) -> Option<Value> {
    let text = std::fs::read(path).ok()?;
    serde_json::from_slice(&text).ok()
}

/// 数出已解析文档里 `key` 下的条目数
fn count_in(doc: Option<&Value>, key: &str) -> usize {
    doc.and_then(|v| outer_collection(v, key))
        .and_then(|v| {
            v.as_object()
                .map(|m| m.len())
                .or_else(|| v.as_array().map(|a| a.len()))
        })
        .unwrap_or(0)
}

/// 前序遍历取首个 object 或 array 类型的 `key` 字段。
///
/// 这些配置里同名键可能既有顶层的一处、又有 `projects.<路径>` 下的一处，取最外层
/// 与 Claude Code 自身的读取优先级一致。
fn outer_collection<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    let mut stack = vec![root];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                if let Some(found) = map.get(key).filter(|v| v.is_object() || v.is_array()) {
                    return Some(found);
                }
                stack.extend(map.values().rev());
            }
            Value::Array(arr) => stack.extend(arr.iter().rev()),
            _ => {}
        }
    }
    None
}

/// 统计目录下含 SKILL.md 的子目录数（跳过隐藏目录，与 shell 的 `*` 通配一致）
fn count_skills(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            !e.file_name().to_string_lossy().starts_with('.') && e.path().join("SKILL.md").is_file()
        })
        .count()
}

/// 复刻 `git symbolic-ref -q --short HEAD || git describe --tags --always`：
/// 从 cwd 逐级向上找 `.git`，读 HEAD。
///
/// 在分支上时 HEAD 是符号引用，取其分支名；detached HEAD 时 HEAD 存的是裸 SHA，
/// 改为找指向该 commit 的 tag（附注 tag 需读对象库解引用），找不到再退回 7 位短
/// SHA。worktree 与 submodule 的 `.git` 是文件，内容形如 `gitdir: <路径>`，
/// 需顺着指向再读其 HEAD。
fn git_branch(start: &Path) -> Option<String> {
    let mut dir = start;
    loop {
        let dot_git = dir.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let target = PathBuf::from(content.trim().strip_prefix("gitdir:")?.trim());
            if target.is_absolute() {
                target
            } else {
                dir.join(target)
            }
        } else {
            dir = dir.parent()?;
            continue;
        };

        let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
        let head = head.trim();
        if let Some(name) = head.strip_prefix("ref: refs/heads/") {
            return Some(name.to_string());
        }
        if !head.is_empty() && head.bytes().all(|b| b.is_ascii_hexdigit()) {
            // detached：有 tag 指向该 commit 就显示 tag 名，没有则退回 7 位短 SHA。
            // 引用与对象都在共享 gitdir（worktree 场景）里找
            let common = common_git_dir(&git_dir);
            return Some(
                find_tag(&common, head).unwrap_or_else(|| head.chars().take(7).collect()),
            );
        }
        return None;
    }
}

/// 在 tag 引用里找指向 `sha` 的那个，同时覆盖松散引用与 packed-refs。
///
/// 松散附注 tag（值是 tag 对象 SHA）借助对象库解引用；打包进 packed-refs 的附注 tag
/// 直接读 `^` 解引发行。tag 对象 delta 存储等解引用不出的情况，由调用方退回短 SHA。
fn find_tag(git_dir: &Path, sha: &str) -> Option<String> {
    if let Some(name) = find_loose_tag(&git_dir.join("refs/tags"), git_dir, sha) {
        return Some(name);
    }

    let text = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    // `^` 行是上一行附注 tag 解引用后的 commit，用 pending 记住那个 tag 名
    let mut pending: Option<&str> = None;
    for line in text.lines() {
        if let Some(peeled) = line.strip_prefix('^') {
            if peeled.trim() == sha
                && let Some(name) = pending
            {
                return Some(name.to_string());
            }
            pending = None;
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let Some((value, name)) = line.split_once(' ') else {
            continue;
        };
        pending = name.strip_prefix("refs/tags/");
        if value == sha
            && let Some(name) = pending
        {
            return Some(name.to_string());
        }
    }
    None
}

/// 递归扫描 refs/tags 下的松散引用，返回相对 `refs/tags` 的 tag 名（tag 名可含 `/`，
/// 如 `release/v2.0.3`）：值直接是 commit SHA（轻量 tag）时直接比较；附注 tag 的候选
/// 收齐后统一解引用——松散对象逐个查，pack 侧批量检索，避免每个 tag 都把全部包扫一遍
fn find_loose_tag(dir: &Path, git_dir: &Path, sha: &str) -> Option<String> {
    let mut annotated: Vec<(String, String)> = Vec::new(); // (tag 名, tag 对象 SHA)
    if let Some(name) = collect_loose_tags(dir, "", sha, &mut annotated) {
        return Some(name);
    }

    let mut candidates: Vec<[u8; 20]> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for (name, object_sha) in &annotated {
        if read_loose_object(git_dir, object_sha)
            .and_then(|object| tag_target(&object))
            .as_deref()
            == Some(sha)
        {
            return Some(name.clone());
        }
        if let Some(sha20) = hex_to_sha(object_sha) {
            candidates.push(sha20);
            names.push(name.clone());
        }
    }

    for (i, object) in read_packed_objects(git_dir, &candidates) {
        if tag_target(&object).as_deref() == Some(sha) {
            return Some(names[i].clone());
        }
    }
    None
}

/// 递归收集松散 tag 引用：值直接命中返回 Some(名)；不能直接判断的记入 pending
fn collect_loose_tags(
    dir: &Path,
    prefix: &str,
    sha: &str,
    pending: &mut Vec<(String, String)>,
) -> Option<String> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // 用 `/` 手工拼接而非 Path::join：git ref 名必须以 `/` 分隔，
        // 而 join 在 Windows 上会给出 `\`
        let full = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            if let Some(hit) = collect_loose_tags(&path, &full, sha, pending) {
                return Some(hit);
            }
            continue;
        }
        // git 更新引用时先写 `<名>.lock` 再改名，忽略避免读到半成品
        if name.ends_with(".lock") {
            continue;
        }
        let Some(content) = std::fs::read_to_string(&path).ok() else {
            continue;
        };
        let content = content.trim();
        if content == sha {
            return Some(full);
        }
        if content.len() == 40 && content.bytes().all(|b| b.is_ascii_hexdigit()) {
            pending.push((full, content.to_string()));
        }
    }
    None
}

/// worktree 的 gitdir（.git/worktrees/<名>）只放本工作树自己的文件（HEAD、index 等），
/// refs 与 objects 都共享主仓 .git，commondir 文件指向那里；普通仓库与 submodule 没有它
fn common_git_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(content) => {
            let common = PathBuf::from(content.trim());
            if common.is_absolute() {
                common
            } else {
                git_dir.join(common)
            }
        }
        Err(_) => git_dir.to_path_buf(),
    }
}

/// 从对象正文里取 tag 指向的目标对象：松散对象带 `<type> <size>\0` 前缀，pack 条目
/// 没有，先剥掉再读首行 `object <sha>`；非 tag 对象返回 None
fn tag_target(object: &[u8]) -> Option<String> {
    let content = match object.iter().position(|&b| b == 0) {
        Some(end) if end < 32 => &object[end + 1..],
        _ => &object[..],
    };
    let first = content.split(|b| *b == b'\n').next()?;
    let target = std::str::from_utf8(first).ok()?.strip_prefix("object ")?;
    let target = target.trim();
    // 目标行必须是完整 SHA；commit 对象首行是 `tree <sha>`，自然不匹配
    (target.len() == 40 && target.bytes().all(|b| b.is_ascii_hexdigit())).then(|| target.to_string())
}

/// 读松散对象：zlib 解压出对象正文。上限 256 字节足够覆盖 tag 对象首行，
/// 超出时解压提前收尾，正文开头仍然完整
fn read_loose_object(git_dir: &Path, sha: &str) -> Option<Vec<u8>> {
    let path = git_dir.join("objects").join(&sha[..2]).join(&sha[2..]);
    inflate::zlib_decompress(&std::fs::read(path).ok()?, 256)
}

/// idx v2 的固定区长度：8 字节 magic + 版本，之后是 256 项 fanout
const IDX_HEADER: usize = 8 + 256 * 4;

/// 批量读取 pack 内对象：每个 idx 只打开一次，先读表头 + fanout 做首字节预筛，
/// 有候选落在包内才整读并在 idx 里逐个二分。返回 (候选序号, 对象正文)。
/// 只支持 idx v2 与完整（非 delta）对象——tag 对象极小，实测各仓库均以完整对象存储；
/// delta 条目会被跳过，由调用方退回短 SHA
fn read_packed_objects(git_dir: &Path, shas: &[[u8; 20]]) -> Vec<(usize, Vec<u8>)> {
    let mut hits = Vec::new();
    let Ok(entries) = std::fs::read_dir(git_dir.join("objects").join("pack")) else {
        return hits;
    };
    for entry in entries.flatten() {
        let idx_path = entry.path();
        if idx_path.extension().and_then(|e| e.to_str()) != Some("idx") {
            continue;
        }
        let mut head = [0u8; IDX_HEADER];
        let Ok(mut file) = File::open(&idx_path) else {
            continue;
        };
        if file.read_exact(&mut head).is_err()
            || head[..4] != [0xff, b't', b'O', b'c']
            || u32::from_be_bytes(head[4..8].try_into().expect("固定 4 字节")) != 2
        {
            continue;
        }
        let fanout = |byte: usize| -> usize {
            u32::from_be_bytes(
                head[8 + byte * 4..12 + byte * 4].try_into().expect("fanout 固定 4 字节"),
            ) as usize
        };
        let has_candidate = shas.iter().any(|sha| {
            let first = sha[0] as usize;
            let lo = if first == 0 { 0 } else { fanout(first - 1) };
            lo != fanout(first)
        });
        if !has_candidate {
            continue;
        }

        let Ok(idx) = std::fs::read(&idx_path) else {
            continue;
        };
        let count = fanout(255);
        for (i, sha) in shas.iter().enumerate() {
            if hits.iter().any(|(hit, _)| *hit == i) {
                continue;
            }
            let first = sha[0] as usize;
            let lo = if first == 0 { 0 } else { fanout(first - 1) };
            let hi = fanout(first);
            if let Some(object) = search_idx(&idx_path, &idx, count, lo, hi, sha) {
                hits.push((i, object));
            }
        }
    }
    hits
}

/// 在整读的 idx 缓冲里二分定位 sha；命中则按偏移读对应 pack 条目
fn search_idx(
    idx_path: &Path,
    idx: &[u8],
    count: usize,
    mut lo: usize,
    mut hi: usize,
    sha: &[u8; 20],
) -> Option<Vec<u8>> {
    while lo < hi {
        let mid = (lo + hi) / 2;
        match idx.get(IDX_HEADER + mid * 20..IDX_HEADER + mid * 20 + 20)?.cmp(&sha[..]) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => {
                let offsets = IDX_HEADER + count * 20 + count * 4;
                let off32 = u32::from_be_bytes(
                    idx.get(offsets + mid * 4..offsets + mid * 4 + 4)?.try_into().ok()?,
                );
                let offset = if off32 & 0x8000_0000 == 0 {
                    off32 as u64
                } else {
                    // 高位为 1 时低 31 位是 64 位偏移表的序号，表紧跟在 32 位偏移表之后
                    let big = offsets + count * 4 + (off32 & 0x7fff_ffff) as usize * 8;
                    u64::from_be_bytes(idx.get(big..big + 8)?.try_into().ok()?)
                };
                return read_pack_entry(&idx_path.with_extension("pack"), offset);
            }
        }
    }
    None
}

/// 读 pack 条目：变长头给出类型与解压后大小，随后是 zlib 流
fn read_pack_entry(pack_path: &Path, offset: u64) -> Option<Vec<u8>> {
    let mut file = File::open(pack_path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;

    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).ok()?;
    let obj_type = (byte[0] >> 4) & 7;
    let mut size = (byte[0] & 0x0f) as u64;
    let mut shift = 4;
    while byte[0] & 0x80 != 0 {
        file.read_exact(&mut byte).ok()?;
        size |= ((byte[0] & 0x7f) as u64) << shift;
        shift += 7;
    }
    // 4 = 完整 tag 对象；6/7 是 delta 存储，放弃（调用方退回短 SHA）
    if obj_type != 4 || size > 64 * 1024 {
        return None;
    }

    // 读取上限：deflate 真实编码不膨胀，2 倍大小足以覆盖并防御损坏头部的谎报
    let mut compressed = Vec::new();
    file.take(size * 2 + 1024).read_to_end(&mut compressed).ok()?;
    // 解压上限取 size + 1：正常流恰好解出 size 字节，能走完 adler32 校验
    let out = inflate::zlib_decompress(&compressed, (size + 1) as usize)?;
    (out.len() as u64 == size).then_some(out)
}

/// 40 位十六进制 SHA-1 转 20 字节；非法输入返回 None
fn hex_to_sha(hex: &str) -> Option<[u8; 20]> {
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut sha = [0u8; 20];
    for (i, byte) in sha.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-19T00:00:00Z
    const SAMPLE_DAY: i64 = 1_789_776_000;

    #[test]
    fn parse_epoch_reads_full_four_digit_year() {
        // 回归：年份必须取 4 位。曾按 2 位取，2026 变成 20，全部记录被判成
        // 早于当日、当日用量恒为 0
        assert_eq!(parse_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_epoch("2026-09-19T00:00:00.000Z"), Some(SAMPLE_DAY));
        assert_eq!(
            parse_epoch("2026-09-19T14:35:17.795Z"),
            Some(SAMPLE_DAY + 14 * 3600 + 35 * 60 + 17)
        );
    }

    #[test]
    fn parse_epoch_rejects_non_utc_and_malformed() {
        // 带偏移的写法必须拒绝：当成 UTC 解析会让日界偏 8 小时
        assert_eq!(parse_epoch("2026-09-19T14:35:17+08:00"), None);
        assert_eq!(parse_epoch("2026-09-19T14:35:17"), None);
        assert_eq!(parse_epoch("2026-09-19"), None);
        assert_eq!(parse_epoch(""), None);
        // 不校验数值范围：数据源固定，出现 13 月这种值本身就不可能，
        // 为它加分支是纯粹的负担
        assert!(parse_epoch("1999-12-31T23:59:59.999Z").is_some());
    }

    #[test]
    fn days_from_civil_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017); // 跨 2000 年闰日
        assert_eq!(days_from_civil(2026, 9, 19), SAMPLE_DAY / SECS_PER_DAY);
    }

    #[test]
    fn local_day_start_sits_on_local_midnight_within_last_day() {
        let start = local_day_start();
        // 加上偏移后应落在 UTC 日界上，即本地 0 点
        assert_eq!((start + LOCAL_OFFSET_SECONDS).rem_euclid(SECS_PER_DAY), 0);
        let now = epoch_secs(std::time::SystemTime::now()).unwrap();
        assert!(start <= now && now - start < SECS_PER_DAY);
    }

    #[test]
    fn keeps_final_snapshot_and_ignores_later_partials() {
        let partial = Usage {
            input_tokens: 100,
            output_tokens: 1,
            ..Default::default()
        };
        let final_ = Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        let mut by_id: HashMap<String, Deduped> = HashMap::new();
        keep_representative(&mut by_id, "m1", partial, false);
        keep_representative(&mut by_id, "m1", final_, true);
        assert_eq!(by_id["m1"].usage.output_tokens, 50);
        // 终态已定后，再来的中间态快照不得把它顶掉
        keep_representative(&mut by_id, "m1", partial, false);
        assert_eq!(by_id["m1"].usage.output_tokens, 50);
    }

    #[test]
    fn picks_largest_output_between_snapshots_of_same_state() {
        // 同为中间态，只有 output 在变——应留下最大的那条
        let snapshot = |output| Usage {
            output_tokens: output,
            ..Default::default()
        };
        let mut by_id: HashMap<String, Deduped> = HashMap::new();
        for output in [7, 9, 5] {
            keep_representative(&mut by_id, "m1", snapshot(output), false);
        }
        assert_eq!(by_id["m1"].usage.output_tokens, 9);
    }

    #[test]
    fn total_sums_all_four_dimensions() {
        let usage = Usage {
            input_tokens: 2826,
            output_tokens: 0,
            cache_read_input_tokens: 3840,
            cache_creation_input_tokens: 100,
        };
        assert_eq!(usage.total(), 6766);
        assert_eq!(Usage::default().total(), 0);
    }

    #[test]
    fn cache_hit_permille_ignores_output() {
        // 分母只有输入三项：命中量不变时，output 涨多少都不该改变命中率
        let lean = Usage {
            input_tokens: 100,
            output_tokens: 0,
            cache_read_input_tokens: 900,
            cache_creation_input_tokens: 0,
        };
        let fat = Usage {
            output_tokens: 900,
            ..lean
        };
        assert_eq!(lean.cache_hit_permille(), Some(900));
        assert_eq!(lean.cache_hit_permille(), fat.cache_hit_permille());
    }

    #[test]
    fn cache_hit_permille_truncates_rather_than_rounds() {
        // 2/3 = 66.666…%，一位小数取 66.6；四舍五入会给出 66.7
        let usage = Usage {
            input_tokens: 1,
            cache_read_input_tokens: 2,
            ..Default::default()
        };
        assert_eq!(usage.cache_hit_permille(), Some(666));
        // 恰好三位整时不得被浮点误差吃掉一位（0.987 类值无法精确表示）
        let exact = Usage {
            input_tokens: 13,
            cache_read_input_tokens: 987,
            ..Default::default()
        };
        assert_eq!(exact.cache_hit_permille(), Some(987));
    }

    #[test]
    fn cache_hit_permille_is_none_when_no_input() {
        // 只有 output 时报 0% 是把「无数据」说成「一次都没命中」，应返回 None
        let output_only = Usage {
            output_tokens: 500,
            ..Default::default()
        };
        assert_eq!(output_only.cache_hit_permille(), None);
        assert_eq!(Usage::default().cache_hit_permille(), None);
    }

    #[test]
    fn usage_accumulates_dimension_wise() {
        let mut acc = Usage::default();
        acc += Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 4,
        };
        acc += Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 30,
            cache_creation_input_tokens: 40,
        };
        assert_eq!(acc.input_tokens, 11);
        assert_eq!(acc.output_tokens, 22);
        assert_eq!(acc.cache_read_input_tokens, 33);
        assert_eq!(acc.cache_creation_input_tokens, 44);
        assert_eq!(acc.total(), 110);
    }

    #[test]
    fn formats_permille_as_one_decimal_percent() {
        assert_eq!(fmt_permille(986), "98.6%");
        assert_eq!(fmt_permille(1000), "100.0%");
        assert_eq!(fmt_permille(5), "0.5%");
        assert_eq!(fmt_permille(0), "0.0%");
    }

    #[test]
    fn keeps_same_message_id_across_files_from_double_counting() {
        // 去重表跨文件共用：会话分叉/续接会把历史记录带进新文件，
        // 同一个 message.id 在第二个文件里再次出现时不得累加第二次
        let mut by_id: HashMap<String, Deduped> = HashMap::new();
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        // 模拟扫两个文件，各自都含同一条记录
        for _ in 0..2 {
            keep_representative(&mut by_id, "msg-shared", usage, true);
        }
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id["msg-shared"].usage.total(), 150);
    }
}
