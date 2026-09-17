//! Claude Code 状态栏：从 stdin 读取会话 JSON，渲染模型、目录、Git 分支、
//! 上下文占用与扩展配置计数。
//!
//! 替代原 bash + jq/grep/awk 实现。原版每次刷新要 fork 七个进程（Windows 上约
//! 0.6 秒），而状态栏每 5 秒刷新一次；这里单进程完成，不产生任何子进程。

mod inflate;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
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
