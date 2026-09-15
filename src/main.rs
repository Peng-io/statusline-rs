//! Claude Code 状态栏：从 stdin 读取会话 JSON，渲染模型、目录、Git 分支、
//! 上下文占用与扩展配置计数。
//!
//! 替代原 bash + jq/grep/awk 实现。原版每次刷新要 fork 七个进程（Windows 上约
//! 0.6 秒），而状态栏每 5 秒刷新一次；这里单进程完成，不产生任何子进程。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

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
        "🤖 {BOLD}{CYAN}{model}{RESET} 📁 {WHITE}{short_dir}{branch}{RESET}{dirs_display}\n"
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

/// 统计 MCP server、hook 事件、skill 数量。
///
/// 配置分四层：全局 `.claude.json`、全局 `settings.json`、项目 `.claude/`、
/// 项目根的 `.mcp.json`。cwd 恰好是配置文件目录本身时跳过项目层，否则会与全局
/// 层重复读取同一文件。
fn counts(cwd: &str) -> (usize, usize, usize) {
    // 未设 CLAUDE_CONFIG_DIR 时全局配置在 home 下，而非 home/.claude 下
    let (claude_dir, global_json) = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            let json = dir.join(".claude.json");
            (dir, json)
        }
        None => {
            let home = std::env::home_dir().unwrap_or_default();
            (home.join(".claude"), home.join(".claude.json"))
        }
    };

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
/// 改为找指向该 commit 的 tag，找不到再退回 7 位短 SHA。worktree 与 submodule 的
/// `.git` 是文件，内容形如 `gitdir: <路径>`，需顺着指向再读其 HEAD。
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
            // detached：有 tag 指向该 commit 就显示 tag 名，没有则退回 7 位短 SHA
            return Some(
                find_tag(&git_dir, head).unwrap_or_else(|| head.chars().take(7).collect()),
            );
        }
        return None;
    }
}

/// 在 tag 引用里找指向 `sha` 的那个，同时覆盖松散引用与 packed-refs。
///
/// 已知局限：松散的附注 tag（refs/tags 下未打包、值是 tag object SHA 的文件）
/// 需要解析对象库才能解引用，这里匹配不到。打包进 packed-refs 的 tag 带 `^` 行
/// 给出 peeled commit，可以正常匹配，而 clone 来的仓库 tag 基本都是这一种。
fn find_tag(git_dir: &Path, sha: &str) -> Option<String> {
    if let Some(name) = find_loose_tag(&git_dir.join("refs/tags"), sha) {
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

/// 递归扫描 refs/tags 下的松散引用，返回相对 `refs/tags` 的 tag 名（tag 名可含
/// `/`，如 `release/v2.0.3`）
fn find_loose_tag(dir: &Path, sha: &str) -> Option<String> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if let Some(found) = find_loose_tag(&path, sha) {
                // 用 `/` 手工拼接而非 Path::join：git ref 名必须以 `/` 分隔，
                // 而 join 在 Windows 上会给出 `\`
                return Some(format!("{name}/{found}"));
            }
        } else if std::fs::read_to_string(&path).is_ok_and(|c| c.trim() == sha) {
            return Some(name);
        }
    }
    None
}
