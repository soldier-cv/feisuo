//! 主题系统回归守卫。
//!
//! 起因是真实缺陷: 浅色主题下按钮渲染成纯白、标题渲染成黑色。
//! 这类问题的根因都是"颜色没走令牌", 但它不会导致编译失败,
//! 只能靠人工审查发现 —— 所以在这里固化成自动检查。
//!
//! 三条守卫:
//! 1. 组件里不得硬编码颜色 (QR code 等规范要求单色的除外);
//! 2. 引用的 `var(--x)` 必须有定义 —— 拼错的令牌会静默失效;
//! 3. 深色令牌必须在浅色主题里也有覆盖 —— 否则浅色下会静默沿用深色值,
//!    这正是"浅色下颜色不对"最隐蔽的一种成因。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// UI 源码根目录 (desktop/src-tauri -> ../../ui/src)
fn ui_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("ui")
        .join("src")
}

/// 收集所有 .vue / .css 文件
fn source_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(source_files(&p));
        } else if matches!(
            p.extension().and_then(|x| x.to_str()),
            Some("vue") | Some("css")
        ) {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// 去掉注释, 避免把说明文字里的 `#xxx` / `var(--xxx)` 当成真实代码。
/// 这是必要的: 注释里经常要举反例, 比如"旧实现写死了 #fff"。
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let bytes: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut in_block = false;
    let mut in_line = false;
    let mut in_str: Option<char> = None;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        if let Some(q) = in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = next {
                    out.push(n);
                    i += 2;
                    continue;
                }
            } else if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if in_block {
            if c == '*' && next == Some('/') {
                in_block = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_line {
            if c == '\n' {
                in_line = false;
                out.push(c);
            }
            i += 1;
            continue;
        }
        match (c, next) {
            // ---- Vue 模板里的 HTML 注释 `<!-- -->` ----
            //
            // 早先只处理 `//` 与 `/* */`，于是**模板注释里的说明文字**
            // 会被当成真实标记。实际踩到的是：为了解释"为什么不能把可点的
            // `<span @click>` 嵌进按钮"，我在注释里**举了这个反例**，
            // 守卫立刻报「App.vue:156 <span @click> 键盘不可达」——
            // 而那个 span 早就不存在了。
            //
            // 与 `/* */` 一样必须跳过：**注释是给人看的，守卫必须当它不存在。**
            //
            // 差别是这里**保留换行**：守卫要按行号报错，
            // 把注释里的换行吃掉会让后面所有行的行号整体偏移
            // —— 那会让另一条按行号定位的守卫一起失准。
            ('<', Some('!'))
                if bytes.get(i + 2) == Some(&'-') && bytes.get(i + 3) == Some(&'-') =>
            {
                let mut j = i + 4;
                let mut kept_newlines = 0usize;
                while j < bytes.len() {
                    if bytes[j] == '-'
                        && bytes.get(j + 1) == Some(&'-')
                        && bytes.get(j + 2) == Some(&'>')
                    {
                        j += 3;
                        break;
                    }
                    if bytes[j] == '\n' {
                        kept_newlines += 1;
                    }
                    j += 1;
                }
                for _ in 0..kept_newlines {
                    out.push('\n');
                }
                i = j;
                continue;
            }
            ('/', Some('/')) => {
                in_line = true;
                i += 2;
                continue;
            }
            ('/', Some('*')) => {
                in_block = true;
                i += 2;
                continue;
            }
            ('\'', _) | ('"', _) => {
                in_str = Some(c);
                out.push(c);
                i += 1;
                continue;
            }
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 找出 `needle` 的所有**字符边界**起始偏移。
/// `str::match_indices` 给的是字节偏移, 直接拿去切含中文的字符串会 panic。
fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while from <= haystack.len() {
        let Some(p) = haystack[from..].find(needle) else {
            break;
        };
        out.push(from + p);
        // 步进 1 个字符而不是 1 个字节, 保证下一次切分仍落在边界上
        from += haystack[from..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
    }
    out
}

#[test]
fn ui_src_directory_must_exist() {
    // 路径算错时上面所有检查都会"零违规通过", 属于最危险的假阴性。
    let root = ui_src();
    assert!(
        root.is_dir(),
        "找不到 UI 源码目录: {} (守卫会静默空跑)",
        root.display()
    );
    // 必须点名关键文件, 只数数量不够: 目录存在但文件被改名/移动时,
    // 数量照样对得上, 守卫却已经检查不到任何东西了。
    for required in ["style.css", "App.vue"] {
        assert!(
            root.join(required).is_file(),
            "缺少关键文件 {}/{}, 主题守卫会静默空跑",
            root.display(),
            required
        );
    }
}

/// 守卫 1: 组件里不得硬编码颜色
#[test]
fn components_must_not_hardcode_colors() {
    let files = source_files(&ui_src());
    let mut violations: Vec<String> = Vec::new();

    for f in &files {
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        // style.css 是令牌定义处, 本身就是一堆 #hex, 天然不在检查范围
        if name == "style.css" {
            continue;
        }
        let raw = std::fs::read_to_string(f).unwrap_or_default();
        let src = strip_comments(&raw);

        // 只检查 <style> 块内部: 模板里的颜色(SVG fill 等)有正当理由
        // (比如 QR code 按规范必须是单色), 不该被这条守卫误伤。
        let body = if name.ends_with(".vue") {
            match src.find("<style") {
                Some(s) => src[s..].to_string(),
                None => continue,
            }
        } else {
            src
        };

        for (i, line) in body.lines().enumerate() {
            let t = line.trim();
            if t.starts_with('@') || t.is_empty() {
                continue;
            }
            if find_hex_color(t) || t.contains("rgb(") || t.contains("rgba(") {
                violations.push(format!("{}:<style>:{}: {}", name, i + 1, t));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "以下位置硬编码了颜色, 浅色主题下必然失效 (应改用 var(--xxx) 令牌):\n  {}",
        violations.join("\n  ")
    );
}

/// 极简 hex 检测: `#` 后跟 3/4/6/8 位十六进制。
/// 不用正则以避免额外依赖。
fn find_hex_color(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len() {
        if chars[i] != '#' {
            continue;
        }
        // 排除 CSS 里的颜色函数写法等情况: # 后必须是十六进制数字
        let mut n = 0;
        while i + 1 + n < chars.len() && chars[i + 1 + n].is_ascii_hexdigit() {
            n += 1;
        }
        if matches!(n, 3 | 4 | 6 | 8) {
            return true;
        }
    }
    false
}

/// 守卫 2: 引用的 var(--x) 必须有定义
#[test]
fn every_referenced_token_must_be_defined() {
    let files = source_files(&ui_src());
    assert!(!files.is_empty(), "没有找到任何 UI 源文件");

    let mut defined: BTreeSet<String> = BTreeSet::new();
    let mut used: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for f in &files {
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(f).unwrap_or_default();
        let src = strip_comments(&raw);

        // 令牌定义: `--name: value`
        // 必须在 **src** 上迭代。早期版本拿 `raw` 的字节偏移去切 `src`,
        // 而两者因去注释而已经是不同的字节序列 —— 遇到中文就panic在
        // "not a char boundary"。这类"扫描器自己先崩"的失败最容易被
        // 误读成"检查通过", 所以这里逐行处理, 天然只落在字符边界上。
        for line in src.lines() {
            let t = line.trim();
            if let Some(colon) = t.find(':') {
                let key = t[..colon].trim();
                if key.starts_with("--") && !key.contains(' ') {
                    defined.insert(key.to_string());
                }
            }
        }

        // 令牌引用: `var(--name`
        for at in find_all(&src, "var(") {
            let tail = &src[at + 4..];
            let end = tail
                .find(|c: char| !(c.is_alphanumeric() || c == '-'))
                .unwrap_or(tail.len());
            let tok = tail[..end].trim().to_string();
            if tok.starts_with("--") {
                used.entry(tok).or_default().insert(name.clone());
            }
        }
    }

    let missing: Vec<String> = used
        .keys()
        .filter(|k| !defined.contains(*k))
        .cloned()
        .collect();

    assert!(
        missing.is_empty(),
        "以下令牌被引用但没有定义, 会静默失效:\n  {}",
        missing
            .iter()
            .map(|m| {
                let files = &used[m];
                format!("{}  <- {}", m, files.iter().cloned().collect::<Vec<_>>().join(", "))
            })
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// 守卫 3: 深色令牌必须在浅色主题也有覆盖
#[test]
fn light_theme_must_override_every_color_token() {
    let css_path = ui_src().join("style.css");
    let css = std::fs::read_to_string(&css_path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}", css_path.display(), e));

    let root_start = css.find(":root").expect("style.css 缺少 :root 块");
    let root_end = css[root_start..].find('}').expect(":root 块未闭合") + root_start;
    let light_start = css
        .find("[data-theme=\"light\"]")
        .or_else(|| css.find("[data-theme='light']"))
        .expect("style.css 缺少浅色主题块");
    let light_end = css[light_start..].find('}').expect("浅色主题块未闭合") + light_start;

    // ⚠️ 必须**先去掉块注释**，再按 : 切键。
    //
    // 早先直接对原文按行切，于是浅色块里那行**注释正文**
    //      都更暗，逐个量下来最低只有 3.53:1 被当成一个"令牌名"，
    // 报告输出成 以下深色令牌在浅色主题里没有覆盖: --bg-row-soft /
    // --bg-active）全都更暗… —— 把注释当令牌名的荒谬结果。
    //
    // 它的真实危害不是难看，而是**让人以为浅色主题漏了一堆颜色覆盖**，
    // 于是开始怀疑守卫本身、甚至把它调宽。假警报比没有守卫更贵。
    let strip_block_comments = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(a) = rest.find("/*") {
            out.push_str(&rest[..a]);
            match rest[a + 2..].find("*/") {
                Some(b) => rest = &rest[a + 2 + b + 2..],
                // 未闭合：保守起见丢弃其后全部，绝不把注释正文当声明
                None => return out,
            }
        }
        out.push_str(rest);
        out
    };

    let collect = |s: &str| -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for line in strip_block_comments(s).lines() {
            let t = line.trim();
            if let Some(colon) = t.find(':') {
                let k = t[..colon].trim();
                if k.starts_with("--") {
                    set.insert(k.to_string());
                }
            }
        }
        set
    };

    let root = collect(&css[root_start..root_end]);
    let light = collect(&css[light_start..light_end]);

    // 字体类令牌在两套主题下本就应该相同, 不要求浅色覆盖
    let theme_agnostic: BTreeSet<String> = ["--font-family", "--font-mono"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let missing: Vec<String> = root
        .iter()
        .filter(|k| !light.contains(*k) && !theme_agnostic.contains(*k))
        .cloned()
        .collect();

    assert!(
        missing.is_empty(),
        "以下深色令牌在浅色主题里没有覆盖, 浅色下会静默沿用深色值 \
         (这正是'浅色主题颜色不对'最隐蔽的成因):\n  {}",
        missing.join("\n  ")
    );
}

/// 圆角必须收敛到**固定的一小组令牌**。
///
/// ## 为什么值得单列一条守卫
///
/// 收敛前样式表里有 12 种圆角（4/5/6/7/8/9/10/12/14/50%/99px/999px）。
/// 相邻容器差 1px 时用户看不出"这是有意的层级"，只看得出"圆角不一致" ——
/// 5px 与 6px 差的那 1px 不传达任何信息。7px 只出现一次（`.scope-mode`）、
/// 9px 只出现在一处"只圆下两角"里：都是随手写的痕迹。
///
/// ## 豁免只有两类，且都是**语义**而非偏好
///
/// - `50%`：圆形头像 / 状态点 / 雷达圈靠它成圆，映射到任何长度值都会变成椭圆。
/// - `0 0 10px 10px`：CSS 多值简写里**插不进变量**
///   （`0 0 var(--radius-lg) var(--radius-lg)` 语法非法 —— var() 整体
///   是一个值，不是可以拆进逗号分隔列表的片段）。这是全表唯一手写值，
///   且必须与 `--radius-lg` 保持一致。
#[test]
fn border_radius_must_use_tokens() {
    let files = source_files(&ui_src());
    let mut strays: Vec<String> = Vec::new();

    for f in &files {
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name == "style.css" {
            continue;
        }
        let raw = std::fs::read_to_string(f).unwrap_or_default();
        let src = strip_comments(&raw);
        let body = if name.ends_with(".vue") {
            match src.find("<style") {
                Some(s) => src[s..].to_string(),
                None => continue,
            }
        } else {
            src
        };
        for (i, line) in body.lines().enumerate() {
            let t = line.trim();
            let Some(rest) = t.strip_prefix("border-radius:") else {
                continue;
            };
            let v = rest.trim().trim_end_matches(';').trim();
            if v.is_empty() || v == "50%" || v == "inherit" || v == "0" {
                continue;
            }
            if v == "0 0 10px 10px" {
                continue;
            }
            if v.starts_with("var(") {
                continue;
            }
            strays.push(format!("{}:<style>:{}: {}", name, i + 1, t));
        }
    }

    assert!(
        strays.is_empty(),
        "以下 border-radius 没用令牌 —— 圆角刻度必须收敛到 \
         --radius-pill / --radius-lg / --radius-md / --radius-sm 四档, \
         否则相邻容器差 1px 会被用户看成『圆角不一致』而不是层级:\n  {}",
        strays.join("\n  ")
    );
}

/// 圆角与"文字变体"令牌必须在两套主题里都有定义。
///
/// 圆角是**纯几何量**，与主题无关，两套主题给同样的值；但仍要求
/// 浅色块里有定义：`var()` 未定义会让声明**直接失效**（不是回落到默认值），
/// 浅色下失效就变成"整个界面变成直角"—— 一眼可见却极难归因。
#[test]
fn radius_and_text_variant_tokens_must_exist_in_both_themes() {
    let css_path = ui_src().join("style.css");
    let css = std::fs::read_to_string(&css_path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}", css_path.display(), e));

    let root_start = css.find(":root").expect("style.css 缺少 :root 块");
    let root_end = css[root_start..].find('}').expect(":root 块未闭合") + root_start;
    let light_start = css
        .find("[data-theme=\"light\"]")
        .or_else(|| css.find("[data-theme='light']"))
        .expect("style.css 缺少浅色主题块");
    let light_end = css[light_start..].find('}').expect("浅色主题块未闭合") + light_start;

    let collect = |s: &str| -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for line in s.lines() {
            let t = line.trim();
            if let Some(colon) = t.find(':') {
                let k = t[..colon].trim();
                if k.starts_with("--") {
                    set.insert(k.to_string());
                }
            }
        }
        set
    };

    let root = collect(&css[root_start..root_end]);
    let light = collect(&css[light_start..light_end]);

    for tok in [
        "--radius-pill",
        "--radius-lg",
        "--radius-md",
        "--radius-sm",
        // 品牌色当**文字**用的两族：压在半透明叠层上 / 压在实心容器上。
        // 漏掉任何一个，浅色主题下状态 chip、面包屑、文件类型图标
        // 的对比度就掉到 2.8~3.3:1 —— 而那正是最需要看清的位置。
        "--accent-on-soft",
        "--info-on-soft",
        "--warn-on-soft",
        "--danger-on-soft",
        "--success-on-soft",
        "--accent-text",
        "--info-text",
        "--warn-text",
        "--danger-text",
        "--success-text",
        "--purple-text",
        "--green-text",
    ] {
        assert!(root.contains(tok), ":root 缺少 {tok}");
        assert!(
            light.contains(tok),
            "浅色主题缺少 {tok} —— var() 未定义会让声明直接失效而不是回落到默认值"
        );
    }
}

/// 守卫 4: 关键前景/背景令牌对必须真的看得清。
///
/// 前面三条守卫只保证"令牌存在且被覆盖", 不保证**读得出来**。
/// 而用户报的正是可读性问题 —— 浅色主题下按钮纯白、标题黑色。
/// 典型成因: 前景色与背景色对调了(白底白字), 或某处沿用了上一套主题的
/// 硬编码色值。前三条都抓不到, 只有算对比度才能抓到。
///
/// 判据采用 WCAG 相对亮度对比度, 阈值 4.5:1 (正文 AA 级标准)。
fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    0.2126 * srgb_to_linear(r as f64 / 255.0)
        + 0.7152 * srgb_to_linear(g as f64 / 255.0)
        + 0.0722 * srgb_to_linear(b as f64 / 255.0)
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let la = relative_luminance(a.0, a.1, a.2);
    let lb = relative_luminance(b.0, b.1, b.2);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// 解析 `#rgb` / `#rrggbb`。只处理不透明色 —— 半透明令牌(track/switch-off 等)
/// 依赖实际叠加底色, 静态算不出结果, 一律跳过而不是给个假结论。
fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    match h.len() {
        3 => {
            let d: Vec<u8> = h.chars().map(|c| c.to_digit(16).unwrap_or(0) as u8).collect();
            Some((d[0] * 17, d[1] * 17, d[2] * 17))
        }
        6 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            Some((
                ((v >> 16) & 0xff) as u8,
                ((v >> 8) & 0xff) as u8,
                (v & 0xff) as u8,
            ))
        }
        _ => None,
    }
}

/// 取出某个主题块里某个令牌的值
fn token_value(css: &str, theme_marker: &str, name: &str) -> Option<String> {
    let start = css.find(theme_marker)?;
    let end = css[start..].find('}')? + start;
    for line in css[start..end].lines() {
        let t = line.trim();
        if let Some(colon) = t.find(':') {
            if t[..colon].trim() == name {
                return Some(t[colon + 1..].trim().trim_end_matches(';').to_string());
            }
        }
    }
    None
}

#[test]
fn key_token_pairs_must_meet_contrast_in_both_themes() {
    let css = std::fs::read_to_string(ui_src().join("style.css"))
        .expect("读取 style.css 失败");

    // (说明, 前景令牌, 背景令牌) —— 这些是界面上"必须有可读对比"的组合。
    // 强调/危险按钮用各自专用的 *-contrast / *-solid 令牌:
    // 压在 --accent / --danger 上的文字色不是 --text-on-solid,
    // 这一点本身就是当初踩坑的地方, 守卫必须按真实用法来写。
    let pairs: Vec<(&str, &str, &str)> = vec![
        ("正文", "--text-primary", "--bg-window"),
        ("次要文字", "--text-secondary", "--bg-card"),
        ("三级文字", "--text-tertiary", "--bg-card"),
        ("卡片正文", "--text-primary", "--bg-card"),
        ("强调色按钮文字", "--accent-contrast", "--accent"),
        ("危险按钮文字", "--text-on-solid", "--danger-solid"),
        ("成功按钮文字", "--text-on-solid", "--success-solid"),
    ];

    const MIN_RATIO: f64 = 4.5;
    let mut failures: Vec<String> = Vec::new();

    for (theme_name, marker) in [("深色", ":root"), ("浅色", "[data-theme=\"light\"]")] {
        for (desc, fg, bg) in &pairs {
            let f = token_value(&css, marker, fg);
            let b = token_value(&css, marker, bg);
            let (Some(f), Some(b)) = (f, b) else {
                // 令牌缺失由守卫 2/3 负责报错, 这里不重复
                continue;
            };
            let (Some(fc), Some(bc)) = (parse_hex(&f), parse_hex(&b)) else {
                // 半透明 / 渐变等无法静态求值, 跳过而不是给假结论
                continue;
            };
            let ratio = contrast_ratio(fc, bc);
            if ratio < MIN_RATIO {
                failures.push(format!(
                    "{} · {}: {} ({}) 压在 {} ({}) 上, 对比度仅 {:.2}:1 < {:.1}:1",
                    theme_name, desc, fg, f, bg, b, ratio, MIN_RATIO
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "以下前景/背景组合对比度不足 (用户会看到'字看不清 / 按钮纯白'):\n  {}",
        failures.join("\n  ")
    );
}

/// 焦点环必须存在, 且在两套主题下都够 3:1。
///
/// ## 为什么值得单列一条守卫
///
/// 改版前整份样式表只有 2 条 `:focus-visible`, 60+ 个可聚焦控件
/// (按钮 / 芯片 / 页签 / 图标按钮 / 面包屑 / 设备行) 全部没有可见
/// 焦点指示。键盘用户 Tab 到哪一项根本看不出来 —— 而这个界面的
/// 主要动作 (选设备、配对、删除) 全在列表和小按钮上, 恰恰最依赖
/// 键盘。
///
/// 这类问题**不会**以"报错"的形式暴露出来: 编译通过、界面能跑、
/// 鼠标用户一切正常。它只在有人用键盘时失效, 而那正是最难被
/// 自动测试之外的手段发现的场景。所以必须写成守卫。
///
/// ## 为什么是 3:1 而不是 4.5:1
///
/// WCAG 1.4.11 Non-text Contrast 管的是"识别 UI 组件与状态所需的
/// 图形" —— 焦点指示器属于这一类, 门槛就是 3:1。4.5:1 是给正文
/// 文字 (1.4.3) 的, 拿来要求焦点环会逼出一圈刺眼的粗边框。
#[test]
fn focus_ring_must_exist_and_meet_non_text_contrast() {
    let css = std::fs::read_to_string(ui_src().join("style.css")).expect("读取 style.css 失败");
    let src = strip_comments(&css);

    // 必须是**全局**兜底规则, 不能只是某个组件的 `:focus-visible`。
    //
    // 只查 `contains(":focus-visible")` 是不够的: 组件里那两条
    // (`.scope-btn` / `.dir-send-btn`) 就能让这个断言通过, 于是把
    // 全局那条改名或删掉 —— 60+ 个控件重新变成没有焦点环 —— 守卫
    // 依然是绿的。判据是"选择器里**没有**类名/属性限定", 也就是
    // 这条规则会命中任意元素。
    let has_global_focus_rule = src.lines().any(|line| {
        let t = line.trim();
        if !t.ends_with('{') || !t.contains(":focus-visible") {
            return false;
        }
        let selectors: Vec<&str> = t.trim_end_matches('{').split(',').map(str::trim).collect();
        selectors.iter().any(|s| {
            !s.contains('.') && !s.contains('#') && !s.contains('[') && !s.is_empty()
        })
    });
    assert!(
        has_global_focus_rule,
        "style.css 里没有全局 :focus-visible 兜底规则 (一个不带类名/属性限定的 \
         选择器) —— 组件里那几条覆盖不到其余 60+ 个控件, 键盘用户将看不到自己停在哪里"
    );

    // 规则**存在**不等于规则**画得出来**。所以还要钉住它的三条声明:
    // 宽度、颜色取自令牌、offset 方向。
    //
    // 为什么值得钉: 这条规则一旦被改成 `outline: none` 或者
    // `outline-color: transparent`, 全局兜底立刻退化成"没有兜底",
    // 而上面那条存在性断言**照样通过** —— 守卫又一次变成了摆设。
    //
    // offset 必须是**负值**(内缩)。本界面大量控件贴边: 侧栏设备行
    // 右侧的隐藏按钮、面包屑、页签。外扩的焦点框会被窗口边界切掉一截,
    // 只显示一半的焦点框比不显示更容易误导。
    let global_body = src
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.ends_with('{')
                && t.contains(":focus-visible")
                && t
                    .trim_end_matches('{')
                    .split(',')
                    .map(str::trim)
                    .any(|s| !s.is_empty() && !s.contains('.') && !s.contains('#') && !s.contains('['))
        })
        .map(|_| ())
        .count();
    assert!(global_body > 0, "全局 :focus-visible 规则定位失败");

    // 取该规则的花括号体
    let body = extract_block_after(&src, ":focus-visible")
        .expect("style.css 里找不到 :focus-visible 规则的声明体");
    let decls = body
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with("/*") && !t.starts_with('*') && !t.is_empty()
        })
        .collect::<Vec<_>>()
        .join(" ");

    assert!(
        decls.contains("outline:")
            && !decls.contains("outline: none")
            && !decls.contains("outline: 0"),
        "全局 :focus-visible 的 outline 被关掉了 —— 等于没有焦点环:\n  {}",
        decls
    );
    assert!(
        decls.contains("var(--focus-ring)"),
        "全局 :focus-visible 的 outline 没走 --focus-ring 令牌 —— \
         两套主题会各自漂移, 且对比度不再被 key_token_pairs 那条守卫覆盖:\n  {}",
        decls
    );
    // 抽 offset 的数值
    let offset = decls
        .split("outline-offset:")
        .nth(1)
        .and_then(|s| s.split(';').next())
        .map(str::trim)
        .unwrap_or("");
    let offset_val: f64 = offset
        .trim_end_matches("px")
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("outline-offset 不是可解析的数值: {:?}", offset));
    assert!(
        offset_val < 0.0,
        "outline-offset 必须是负值(内缩), 当前是 {}px —— 外扩的焦点框会被窗口边界切掉",
        offset_val
    );
    assert!(
        decls.contains("solid") || decls.contains("var("),
        "outline 必须是实线或用令牌表达的线型, 当前声明: {}",
        decls
    );

    let pairs: Vec<(&str, &str, &str)> = vec![
        ("焦点环 / 窗口底", "--focus-ring", "--bg-window"),
        ("焦点环 / 卡片", "--focus-ring", "--bg-card"),
        ("焦点环 / 侧栏", "--focus-ring", "--bg-sidebar"),
    ];

    const MIN_RATIO: f64 = 3.0;
    let mut failures: Vec<String> = Vec::new();

    for (theme_name, marker) in [("深色", ":root"), ("浅色", "[data-theme=\"light\"]")] {
        for (desc, fg, bg) in &pairs {
            let (Some(f), Some(b)) = (
                token_value(&css, marker, fg),
                token_value(&css, marker, bg),
            ) else {
                failures.push(format!("{} · {} 缺少令牌", theme_name, desc));
                continue;
            };
            let (Some(fc), Some(bc)) = (parse_hex(&f), parse_hex(&b)) else {
                continue;
            };
            let ratio = contrast_ratio(fc, bc);
            if ratio < MIN_RATIO {
                failures.push(format!(
                    "{} · {}: {} ({}) 压在 {} ({}) 上, 对比度仅 {:.2}:1 < {:.1}:1",
                    theme_name, desc, fg, f, bg, b, ratio, MIN_RATIO
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "焦点环在某些底色上不可见 (WCAG 1.4.11 要求 3:1):\n  {}",
        failures.join("\n  ")
    );
}

/// 取 `needle` 首次出现处之后那一对花括号之间的内容。
///
/// 只处理"选择器行独占一行、以 `{` 结尾"这种形态 —— 样式表里绝大多数
/// 规则都是这样写的。找不到时返回 `None`, 由调用方给出明确报错
/// (而不是 panic 出一句看不懂的 unwrap 失败)。
fn extract_block_after(src: &str, needle: &str) -> Option<String> {
    let at = src.find(needle)?;
    let rest = &src[at..];
    let open = rest.find('{')?;
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[open + 1..open + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// 令牌表里不许有**零引用**的令牌。
///
/// ## 这是 `every_referenced_token_must_be_defined` 的镜像
///
/// 那条守的是"引用了但没定义"（`var()` 未定义会让声明**直接失效**）。
/// 这条守"定义了但没人引用" —— 危害方向不同，但同样致命：
///
/// - 双主题文件里，一个没人用的令牌要**写两遍**。改深色那份时浅色那份
///   忘了改, 没有任何东西会察觉 —— 直到哪天有人开始用它。
/// - 更要命的是"看起来像在用"。本轮实测抓到 7 个零引用令牌，其中
///   `--info` / `--success` / `--purple` / `--green` 让令牌表看上去
///   有一套完整的语义色板（danger/warn/info/success/purple/green），
///   而真正在用的只有 `--danger` 与 `--warn` 两个, 其余一律走
///   `-soft` / `-on-soft` / `-text` 三档。下一个人要"给成功态换个绿"
///   很自然会写 `var(--success)` —— 那条颜色**不在任何对比度守卫的
///   测量范围内**，于是可能直接上线一份读不清的文字。
/// - `--border-focus` 是同类陷阱的极端: 它名字就叫 focus, 而同文件里
///   真正在用的焦点环是 `--focus-ring`（实心值，7.4:1）。前者只有
///   2.2:1。谁按名字选谁，就会做出一份**合规但看不见**的焦点环。
#[test]
fn no_unused_design_tokens() {
    let css_path = ui_src().join("style.css");
    let css_raw = std::fs::read_to_string(&css_path).expect("读取 style.css 失败");
    let css = strip_comments(&css_raw);

    // 只看 :root —— 浅色块里的同名项是"覆盖"，不是新令牌。
    let root_start = css.find(":root").expect("style.css 缺少 :root 块");
    let root_end = css[root_start..].find('}').expect(":root 块未闭合") + root_start;

    let defined: Vec<String> = css[root_start..root_end]
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            if !t.starts_with("--") {
                return None;
            }
            let (k, _) = t.split_once(':')?;
            let k = k.trim();
            if k.contains(' ') {
                None
            } else {
                Some(k.to_string())
            }
        })
        .collect();

    // 引用来自**所有** UI 源文件，且包含 style.css 的规则体
    // （`::-webkit-scrollbar-thumb { background: var(--scrollbar-thumb) }`
    //  就只出现在规则体里，只扫 `--x: value` 定义行会漏掉它，
    //  进而把这个明明在用的令牌误报成"零引用"）。
    //
    // ⚠️ 必须喂**去注释后**的文本。裸读原始文件时, 上一步刚写下的那句
    // 解释性注释("很自然会写 var(--success)")会被当成真引用 ——
    // 于是这条守卫对自己的删除说明言听计反, 把刚删的令牌又判成"在用"。
    // 这已是本项目第四次栽在"注释里出现要检查的模式"上
    // (前三次见 strip_comments 的文档与本文件早期的事故记录),
    // 所以规则很硬: **任何扫描都必须先 strip_comments**, 没有例外。
    let mut haystack = css.clone(); // 已去注释；规则体保留
    for f in source_files(&ui_src()) {
        if f == css_path {
            continue;
        }
        if let Ok(s) = std::fs::read_to_string(&f) {
            haystack.push('\n');
            haystack.push_str(&strip_comments(&s));
        }
    }

    let unused: Vec<String> = defined
        .iter()
        .filter(|name| {
            // ⚠️ 括号必须写全: `var(--x)` 而不是 `var(--x`。
            //
            // 漏掉右括号时, `--success` 会被 `--success-soft` /
            // `--success-text` / `--success-solid` 这些**前缀相同**的令牌
            // 的引用救活, 于是判成"在用" —— 而它其实零引用。
            // 本条守卫正是被自己的变异测试逼出这个坑的: 加回 `--success`
            // 它毫无反应, 逐层打印才发现匹配串是 `var(--success`(无括号)。
            //
            // 前缀相同是这类令牌表的基本形态(`-soft` / `-on-soft` /
            // `-text` / `-border` / `-strong` 各一档), 所以这不是偶发而是必然:
            // 只要存在一个"裸"令牌, 它就会被自己的一堆变体遮住。
            //
            // 另外 var(--name ...) 与 JS 里以字符串形式出现的 '--name'
            // 两种写法都要认, 前者是 CSS, 后者见 deviceStore/main.ts。
            let as_var = format!("var({})", name);
            let as_str = format!("\"{}\"", name);
            let as_str2 = format!("'{}'", name);
            !(haystack.contains(&as_var)
                || haystack.contains(&as_str)
                || haystack.contains(&as_str2))
        })
        .cloned()
        .collect();

    assert!(
        unused.is_empty(),
        "以下令牌**零引用**（定义了却没人用）:\n  {}\n  \
         危害不是多余, 而是双主题下改一边忘了另一边没人会发现, \
         且下一个人很可能按名字选中一个**不受对比度守卫覆盖**的颜色。\n  \
         要么用起来, 要么删掉（两套主题都要删）。",
        unused.join("\n  ")
    );
    let _ = (defined, unused);
}

/// 「清空全部」必须同时清理**两个**暂存目录。
///
/// ## 这条守卫挡的是什么
///
/// 暂存区有两代位置，是一次安全整改留下的：
///
/// - 旧的 `receive_dir/.feisuo-staging` —— 用户**可见**目录，`stage_temp_payload` 写；
/// - 新的 `app_dir/clipboard-staging` —— app 私有目录，`stage_clipboard_payload` 写。
///
/// 发送成功后的清理一直是两条都调（`cleanupTempPayloads` +
/// `cleanupClipboardStaging`），**唯独「清空全部」只调了第一条**。
/// 而剪贴板内容恰恰只落在**新**目录 —— 于是用户点「清空全部」，
/// 明文截图/密码仍然留在磁盘上，只能等 24h TTL 清理。
///
/// ## 为什么它不会自己暴露
///
/// 界面表现完全正常：清单空了，没有报错，也没有异常日志。
/// 用户以为内容已删干净。**隐私类缺陷里最坏的一类是"删了但没删掉"** ——
/// 因为它的症状是"我以为安全了"。
#[test]
fn clear_all_must_clean_both_staging_locations() {
    let vue = std::fs::read_to_string(ui_src().join("App.vue")).expect("读取 App.vue 失败");
    let src = strip_comments(&vue);

    let body = match src.find("function clearStaged(") {
        Some(s) => &src[s..],
        None => panic!("App.vue 里找不到 clearStaged()"),
    };
    let end = body
        .find("\n}")
        .expect("clearStaged() 函数体未闭合");
    let fn_body = &body[..end];

    // 旧位置（收件目录）
    assert!(
        fn_body.contains("cleanupTempPayloads"),
        "clearStaged() 不再清理旧暂存目录 (receive_dir/.feisuo-staging) —— \
         上一版本暂存的文件会永久残留"
    );
    // 新位置（app 私有目录）—— 这条正是本守卫存在的理由
    assert!(
        fn_body.contains("cleanupClipboardStaging"),
        "clearStaged() 没有清理 **新** 暂存目录 (app_dir/clipboard-staging) —— \
         而剪贴板内容恰恰只落在这里。于是用户点「清空全部」后, \
         明文截图/密码仍留在磁盘上, 界面却显示已清空。\
         必须与发送成功后的清理保持同一套: 两条都调, 且按路径精确删。"
    );
    // 按路径精确删: 全量清空会误删用户排队等待发送的内容。
    //
    // 判据必须是**收集临时路径的那一句本身**，而不是"函数体里出现过
    // tempPaths 这个词" —— 后者会被下面 `tempPaths.length` 的**使用处**
    // 满足，于是"删掉收集、只留使用"这种改法照样通过(虽然 vue-tsc 会
    // 因变量未定义而报错，但那时守卫已经放行了)。
    assert!(
        fn_body.contains(".filter((f) => f.temporary)"),
        "clearStaged() 没有按路径精确删除 —— 全量清空会把用户排队中尚未发送的\
         剪贴板内容一起删掉 (静默丢数据)。必须先从 stagedFiles 里筛出 temporary 的路径。"
    );
}

/// 暂存区 TTL 必须在**运行期周期性**执行，不能只在启动时清一次。
///
/// ## 为什么这条不能省
///
/// DESIGN_TRUST_SHUTTLE §6.5 要求「TTL 24h 自动清理 + 启动时清理 + 退出时清理」。
/// 曾经只实现了"启动时清理"（前端 `onMounted` 调一次）。而飞梭是**托盘常驻**
/// 应用，可能连续运行数周不重启，于是：
///
/// ```text
/// 第 0 天 09:00  启动（清了当时的东西）
/// 第 0 天 10:00  复制含密码的截图 → 发送剪贴板 → 关掉预览没发
/// 第 0 ~ 21 天   应用一直运行 —— 文件一直不删
/// 第 21 天       才等到重启时被清掉
/// ```
///
/// 也就是「24 小时 TTL」实际兑现成了「下次重启时才删」。这与当初把暂存区
/// 迁到 app 私有目录所要解决的隐私问题**直接矛盾**：文档的原话是
/// "剪贴板里常混着密码与验证码，无限期保留是隐私漏洞而不是便利"。
///
/// ## 为什么判据要求它在 Rust 侧
///
/// 前端 `setInterval` 在窗口隐藏时会被节流甚至暂停，而托盘常驻恰好意味着
/// 窗口长期隐藏；webview 崩溃时定时器也没了。宿主进程的定时任务不受影响。
#[test]
fn clipboard_staging_ttl_must_run_periodically_not_only_at_startup() {
    let main_rs = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("main.rs"),
    )
    .expect("读取 main.rs 失败");
    let src = strip_comments(&main_rs);

    assert!(
        src.contains("purge_stale_staging"),
        "main.rs 里没有周期性的 purge_stale_staging 调用 —— \
         暂存区 TTL 只在启动时清一次, 对托盘常驻应用等于形同虚设"
    );
    assert!(
        src.contains("tokio::time::interval") || src.contains("tokio::time::sleep"),
        "main.rs 里的暂存清理不是定时任务 —— 必须是周期执行, 不能是启动时跑一次"
    );
    // 必须是"周期"而不是"启动后 sleep 一次再进循环"：后者在退出前不会重复。
    let periodic = src.matches("purge_stale_staging").count();
    assert!(
        periodic >= 1,
        "预期 main.rs 至少有一处 purge_stale_staging"
    );

    // 间隔必须显著小于 TTL 本身，否则等于没有周期清理。
    //
    // 解析 `Duration::from_secs(...)` 的实参。刻意支持 `30 * 60` 这种
    // 可读写法 —— 只认纯整数会让守卫在有人把 `1800` 改写成 `30 * 60`
    // （完全等价、只是更好读）时突然报错，那是"守卫制造噪声"而不是
    // "守住了不变量"。解析不了就当没找到，交给下面的断言报错。
    let eval_secs = |expr: &str| -> Option<u64> {
        let expr = expr.trim();
        if let Ok(v) = expr.parse::<u64>() {
            return Some(v);
        }
        // 只支持纯乘法: 逐项解析后连乘
        let mut total: u64 = 1;
        for part in expr.split('*') {
            total = total.checked_mul(part.trim().parse::<u64>().ok()?)?;
        }
        Some(total)
    };

    let secs: Vec<u64> = src
        .match_indices("from_secs(")
        .filter_map(|(i, _)| {
            let tail = &src[i..];
            let start = tail.find('(')? + 1;
            let end = tail[start..].find(')')? + start;
            eval_secs(&tail[start..end])
        })
        .collect();
    assert!(
        secs.iter().any(|s| *s <= 6 * 3600),
        "找不到 <= 6 小时的清理间隔（从 main.rs 解析到的 from_secs 值: {:?}）—— \
         间隔若接近甚至超过 24h TTL, 周期性清理就失去意义",
        secs
    );
}

/// 手工输入 IP 的两个入口都必须做**格式**校验，不能只判空。
///
/// ## 为什么值得单列一条守卫
///
/// 此前「直连对端 IP」与「配对时手填对端 IP」都只判 `if (!ip) return`，
/// 于是输入 `abc` / `192.168.1` / `999.1.1.1` 也能按下按钮，
/// 然后等一次**必然失败**的网络探测才把后端的原始错误显示出来。
///
/// 用户看到的是"连接失败"，而他真正需要知道的是"你输的不是 IP" ——
/// 后者才是他能照着改的那件事。
///
/// 关键在于这条缺陷**完全不会报错**：按钮可点、请求能发出、错误也能显示，
/// 一切"看起来正常"。只有把"失败原因是否可行动"当验收标准时才看得见。
///
/// 判据要求三件事同时成立：
/// 1. 有行内提示（`field-hint`），而不是只在提交后弹一次 toast；
/// 2. 按钮 `disabled` 绑定了格式状态 —— 明知会失败还让点，就是拿一次
///    网络往返换一个没法照着改的错误;
/// 3. 提交函数里再挡一道 —— `@keyup.enter` 直接调函数，不经过按钮的 disabled。
#[test]
fn manual_ip_entry_must_validate_format_not_just_emptiness() {
    let vue = std::fs::read_to_string(ui_src().join("App.vue")).expect("读取 App.vue 失败");
    let src = strip_comments(&vue);

    assert!(
        src.contains("function isValidIpv4"),
        "App.vue 里没有 isValidIpv4 —— 手工输入的 IP 没有格式校验"
    );

    // 两个提交函数都要有格式校验（不只是判空）
    for fn_name in ["function submitDirectConnect(", "async function submitPairing("] {
        let Some(s) = src.find(fn_name) else {
            panic!("找不到 {}", fn_name);
        };
        let body = &src[s..];
        let end = body.find("\n}").expect("函数体未闭合");
        let f = &body[..end];
        assert!(
            f.contains("isValidIpv4"),
            "{} 缺少 isValidIpv4 校验 —— 只判空的话, 手打的非法 IP 会换来一次\
             注定失败的网络探测, 用户看到的是无法照着改的『连接失败』",
            fn_name
        );
    }

    // 行内提示：两个输入框都要有
    let hints = src.matches("field-hint is-error").count();
    assert!(
        hints >= 2,
        "行内格式提示只有 {} 处 —— 直连与配对两个入口都应各自有, \
         因为它们的错误文案与使用时机不同",
        hints
    );

    // 提示要能被辅助技术关联到**各自的**输入框。
    //
    // ⚠️ 判据必须点名具体的绑定，不能去数 `aria-describedby` 出现了几次 ——
    // 本文件里还有审批弹窗的 `aria-describedby="approval-dialog-desc"`，
    // 删掉两个 IP 输入框里的任意一处，计数依然 ≥ 2，守卫照样放行。
    // 数"出现次数"会把**无关用法**算进来，这是这一类判据的通病。
    for binding in [
        "aria-describedby=\"directConnectIpHint",
        "aria-describedby=\"pairTargetIpHint",
        "aria-invalid=\"!!directConnectIpHint",
        "aria-invalid=\"!!pairTargetIpHint",
    ] {
        assert!(
            src.contains(binding),
            "缺少 `{}` —— 行内提示没有关联到对应的输入框, \
             屏幕阅读器用户看不到它（审批弹窗上那处 aria-describedby \
             与这两个入口无关，不能替代）",
            binding
        );
    }

    // 校验器必须真的逐段判 <= 255，而不是只匹配点分十进制的形状
    //
    // ⚠️ 分步写，别写成 `src[a..src[a..].find(x) + 2]` ——
    // `+` 比 `[]` 结合得更紧，那样算出来的是 `src[a..(find结果+2)]`，
    // 而 find 结果是**相对 a** 的偏移，切片长度会变成几十字节甚至 panic。
    let vstart = src.find("function isValidIpv4").expect("isValidIpv4 不存在");
    let vtail = &src[vstart..];
    let vend = vtail.find("\n}").expect("isValidIpv4 函数体未闭合") + 2;
    let vbody = &vtail[..vend];
    assert!(
        vbody.contains("255"),
        "isValidIpv4 里看不到 255 —— 只匹配形状的话 999.1.1.1 会被当成合法 IP"
    );
}

/// 承载**远端可控文本**的 flex 子项必须显式处理溢出。
///
/// ## 为什么值得单列一条守卫
///
/// `text-overflow: ellipsis` 只作用于元素**自己**的 inline 内容。
/// 父元素一旦是 flex 容器，裸文本会被拆成匿名 flex item ——
/// 父元素上的 ellipsis 对它完全无效，而匿名 flex item 的
/// `min-width: auto` 又禁止它收缩。长文本于是把后面的兄弟节点
/// （角标、按钮）挤出容器，再被 `overflow: hidden` 裁掉。
///
/// 真实缺陷（`.pane-name`）：设备名来自**对端**，长度不可控。
///
/// ## 四象限实测，别凭直觉写判据
///
/// 用从 App.vue **原样抽取**的规则搭对照页，容器右边界 x=310，
/// 看 `.pane-count` 的右缘落在哪：
///
/// | 面板 | 改动                          | 右缘  | 角标     |
/// |------|-------------------------------|-------|----------|
/// | A    | 改动前：裸文本、count 无 flex   | 438   | 不可见   |
/// | B    | **只**给 count 加 flex:0 0 auto | 438   | 不可见   |
/// | C    | **只**把文本包进 span          | 271   | 可见     |
/// | D    | 两者都做                       | 271   | 可见     |
///
/// B 与 A 完全一致 —— `flex` 属性单独加是**无效**的，真正起作用的只有
/// 「把裸文本包进独立元素」。这条结论是写出来的注释给出的**反面教训**：
/// 一开始这里写的是"三件事必须同时成立"，被 B、C 两格实测推翻了。
/// 下面 `.pane-count` 的 `flex: 0 0 auto` 因此只是纵深防御，
/// 守卫仍要求它存在（作为约定），但注释不再声称它是本缺陷的修复手段。
///
/// 关键在于这个缺陷**不报错**：界面只是"少了个角标"，没有任何异常信号。
#[test]
fn flex_children_carrying_remote_text_must_handle_overflow() {
    let vue = std::fs::read_to_string(ui_src().join("App.vue")).expect("读取 App.vue 失败");
    let src = strip_comments(&vue);

    // 设备名是对端可控、长度不可限的，必须落在显式元素里。
    // ⚠️ 判据点名 .pane-name-text，不能只 checks("text-overflow")：
    // 文件里已有 15 处 text-overflow，改错地方照样通过。
    assert!(
        src.contains("class=\"pane-name-text\""),
        "找不到 .pane-name-text —— 设备名回到了裸文本，flex 匿名项会把它后面的 \
         计数角标挤出容器再裁掉（四象限 A：角标右缘 438 > 容器 310）"
    );

    // 两个面板都要包（本机卷标 / 对端设备名）
    let mut i = 0;
    let mut panes = 0;
    while let Some(at) = src[i..].find("<div class=\"pane-name\">") {
        let start = i + at;
        let tail = &src[start..];
        let end = tail.find("</div>").expect("pane-name 未闭合");
        let block = &tail[..end];
        panes += 1;
        assert!(
            block.contains("<span class=\"pane-name-text\""),
            "第 {} 个 .pane-name 里的文本没有包在 .pane-name-text 里 —— \
             flex 匿名项的 min-width:auto 会拒绝收缩，长名字把 .pane-count 挤出容器",
            panes
        );
        i = start + end;
    }
    assert!(panes >= 2, "只找到 {} 个 .pane-name，本机/对端两个面板都应有", panes);

    // 省略号三件套，缺一不可（这里才是真正必需的那部分）
    let tstart = src.find(".pane-name-text {").expect("没有 .pane-name-text 规则");
    let ttail = &src[tstart..];
    let tblock = &ttail[..ttail.find("}").expect("未闭合") + 1];
    for decl in ["min-width: 0", "overflow: hidden", "text-overflow: ellipsis"] {
        assert!(
            tblock.contains(decl),
            ".pane-name-text 缺少 `{}` —— 少这一条，省略号就不会出现",
            decl
        );
    }

    // 角标不许被伸缩：这是**约定**（防将来再加一个可伸缩兄弟），
    // 四象限 B 已证明它不是本缺陷的修复手段，所以这里不写成"否则会坏"。
    let cstart = src.find(".pane-count {").expect("没有 .pane-count 规则");
    let ctail = &src[cstart..];
    let cblock = &ctail[..ctail.find("}").expect("未闭合") + 1];
    assert!(
        cblock.contains("flex: 0 0 auto"),
        ".pane-count 缺少 `flex: 0 0 auto` —— 计数角标是信息本身，\
         按约定它不参与伸缩（注意：这不是长名字溢出缺陷的修复手段）"
    );
}

/// 文档标题层级必须是 h1 → h2 → h3，不能跳级，也不能缺顶层。
///
/// ## 为什么值得单列一条守卫
///
/// 改之前的实测大纲：
/// ```text
/// (没有 h1)
/// h3 传输记录          ← 视图标题占了 h3
///   h4 安全/受信设备…  ← 区块跟着降一级
/// h3 系统设置
/// h3 把文件拖到这里    ← 从 h1 直接跳到 h3
/// h4 正在搜索附近设备  ← 会消失的状态提示，污染大纲
/// ```
///
/// 三条真实缺陷：
/// 1. **没有 h1**：按标题跳转的用户拿不到任何顶层锚点，
///    也无从判断自己在哪个应用里。
/// 2. **跳级**：视图标题占 h3、其区块占 h4，h2 这一层整层缺失。
/// 3. **幽灵标题**：`正在搜索附近设备…` 是**会变的状态**，设备一搜到
///    整个元素就消失 —— 作为标题会在大纲里凭空出现又凭空消失。
///
/// 第 4 个视图（双栏穿梭）原本**完全没有标题**，导致按标题跳转时
/// 四个视图里只有两个能到。给它补了视觉隐藏的 `h2`：页签名已经在
/// 页签上写着，再抄一遍可见文字是重复信息。
#[test]
fn document_heading_hierarchy_must_not_skip_levels() {
    let vue = std::fs::read_to_string(ui_src().join("App.vue")).expect("读取 App.vue 失败");
    // 分区扫描，各去各的注释（理由见 strip_css_comments 的注释）
    let clean = strip_html_comments(&vue);
    let tpl = {
        let end = clean.find("<script").unwrap_or(clean.len());
        &clean[..end]
    };
    let css = strip_css_comments(&section(&clean, "<style", "</style>"));

    // 不能用 scan_open_tags：它要求标签的 `>` 后面是空白/`<`/`{`，
    // 于是 `<h2>传输记录</h2>` 这种**紧跟文字**的标题一个都扫不到
    // （实测就是 0 个）。这里用引号感知的扫描。
    let headings = scan_headings_with_parents(tpl);
    let tags: Vec<(usize, String, String)> = headings
        .iter()
        .map(|(t, a, _)| (0usize, t.clone(), a.clone()))
        .collect();

    // ---- 1. 恰好一个 h1，且是应用名 ----
    let h1: Vec<_> = tags.iter().filter(|(_, t, _)| t == "h1").collect();
    assert_eq!(
        h1.len(),
        1,
        "全篇应有且仅有一个 h1，实得 {} 个 —— 按标题跳转的用户需要顶层锚点，\
         多个 h1 会让「文档标题」变成普通段落",
        h1.len()
    );
    assert!(
        h1[0].2.contains("brand-text"),
        "h1 应当是标题栏的应用名（.brand-text），实得属性：{}",
        h1[0].2
    );

    // ---- 2. 四个视图：各有一个 h2，且视图内部不跳级 ----
    //
    // ⚠️ 这里**不能**按整份模板的源码顺序走一遍标题层级。
    // 本文件把所有 `v-if` 弹窗写在模板**开头**，所以源码顺序是
    // 「h1 → 弹窗 h3 → 弹窗 h3 → … → 视图 h2」，直接按顺序判会得到
    // 「h1 跳到 h3」的假报警。
    //
    // 更本质的原因：**弹窗的标题不参与页面大纲**。它是
    // `role="dialog" + aria-labelledby` 的命名来源，作用域是该弹窗本身；
    // 而弹窗是被 `v-if` 条件渲染的，源码顺序本来就不等于无障碍树顺序。
    // 所以按视图分区判，才是对用户真正有意义的那个约束。
    for view in ["view-send", "view-shuttle", "view-log", "view-settings"] {
        let anchor = format!("class=\"tab-page {}", view);
        assert!(
            tpl.contains(&anchor),
            "找不到视图容器 {}",
            anchor
        );
        let at = tpl.find(&anchor).expect("视图容器不存在");
        // 从 anchor **之后**开始找下一个 tab-page。
        // ⚠️ 不能从 `at` 开始找：`class="tab-page view-send"` 本身就含
        // `tab-page `，偏移 7 就命中，于是 block 被截成 7 个字符，
        // 扫出来"一个标题都没有" —— 一条看起来像真 bug 的假报警。
        let after = at + anchor.len();
        let end = tpl[after..]
            .find("tab-page ")
            .map(|n| after + n)
            .unwrap_or(tpl.len());
        let block = &tpl[at..end];
        let inner = scan_headings_with_parents(block);
        assert!(
            !inner.is_empty(),
            "视图 {} 里一个标题都没有 —— 按标题跳转的用户到不了这个页。\
             可见页标题用 .header-titles h2，没有可见标题的用 .view-heading-sr",
            view
        );
        assert_eq!(
            inner[0].0, "h2",
            "视图 {} 的第一个标题应是 h2（视图层级），实得 <{}>",
            view, inner[0].0
        );
        let mut prev = 2u8;
        for (tag, _, _) in inner.iter().skip(1) {
            let lvl = tag[1..].parse::<u8>().expect("标题标签形如 h2");
            assert!(
                lvl <= prev + 1,
                "视图 {} 内部标题从 h{} 跳到 <{}> —— 中间缺了一层，大纲会缺一级",
                view,
                prev,
                tag
            );
            prev = lvl;
        }
    }

    // ---- 4. 会变的态提示不该是标题 ----
    assert!(
        !tpl.contains("<h4>正在搜索附近设备"),
        "「正在搜索附近设备…」被标成了 h4 —— 它是**会消失的状态**（设备一搜到\
         整个元素就没了），作为标题会在大纲里凭空出现又凭空消失。\
         应当是 role=\"status\" 的段落"
    );

    // ---- 5. 视觉隐藏必须真的还在无障碍树里 ----
    assert!(
        tpl.contains("class=\"view-heading-sr\""),
        "找不到 .view-heading-sr —— 没有可见页标题的两个视图无法被按标题跳转访问"
    );
    let vstart = css.find(".view-heading-sr {").expect("没有 .view-heading-sr 规则");
    let vtail = &css[vstart..];
    let vblock = &vtail[..vtail.find('}').expect("未闭合") + 1];
    for decl in ["position: absolute", "overflow: hidden", "clip-path: inset(50%)"] {
        assert!(
            vblock.contains(decl),
            ".view-heading-sr 缺少 `{}` —— 视觉隐藏的标准三件套",
            decl
        );
    }
    assert!(
        !vblock.contains("display: none") && !vblock.contains("visibility: hidden"),
        ".view-heading-sr 不能用 display:none / visibility:hidden —— \
         那会把元素从无障碍树里摘掉，等于没加"
    );
}

/// CSS 里凡是「类名 + 标题标签」的选择器，标签必须与模板里的实际标签一致。
///
/// ## 为什么值得单列一条守卫
///
/// 这是本轮改动里最隐蔽的一种回归：把 `h3` 改成 `h2` 之后，
/// CSS 里的 `.header-titles h3` **不会报错**，只是从此不匹配任何元素，
/// 16px 字号静默退回浏览器默认的 1.5em —— 界面只是"字突然变大了"，
/// 既不崩溃也不告警，肉眼极易漏掉（而且往往正好是没人盯着的那一屏）。
///
/// 所以这里做的是**通用**检查：把 CSS 规则里所有同时出现类名与标题标签的
/// 复合选择器抽出来，逐个核对模板里是否真的存在「那个类 + 那个标签」。
/// 以后再改任何标题标签，这里都会立刻报警。
///
/// 不引 `regex` 依赖 —— 为了一条测试守卫给桌面端加正则库不划算，
/// 去掉 Vue 模板里的 `<!-- -->` 注释。
///
/// ⚠️ `strip_comments` 只处理 `//` 与 `/* */`，**不处理 HTML 注释**。
/// 这在本轮咬过一次：我给 h1 写的说明注释里含 `* { margin: 0 }`，
/// `css_heading_pairs` 把这对花括号当成规则边界，于是把注释里那句
/// `.titlebar-drag-area 是 flex 容器` 连同同段的 `h1` 当成了一条
/// `.class hN` 选择器，凭空报出一条不存在的失配。
///
/// 这就是"注释里出现要检查的模式"的又一次翻版 —— 前几轮栽在
/// `//` 与 `/* */` 上，这次换成 `<!-- -->`。凡是**按结构扫描**的守卫，
/// 都得先把这三种注释一起去掉。
/// 只去掉 CSS 里的 `/* ... */` 块注释。
///
/// ## 为什么不用共享的 `strip_comments`
///
/// `strip_comments` 是给 **JS** 写的状态机：把 `'`/`"` 当字符串定界、
/// 把 `//` 当行注释。把它套在 Vue 模板或整份 App.vue 上会出事 ——
/// 实测 `css_heading_pairs` 解析出的「.class hN」选择器数量：
///
/// ```text
/// 完全不剥         12 条（其中有注释文字造成的假阳性）
/// 只剥 HTML 注释    8 条
/// 只剥 strip_comments  4 条
/// 两种都剥          0 条  ← 守卫直接退化成空壳
/// ```
///
/// 模板里有 324 对 `{}`（全是 `{{ }}` 插值），JS 状态机在里面被
/// 引号与 `//` 搅乱后，深度计数就再也不回零了。
///
/// 所以这里按**分区**处理：模板区只去 HTML 注释，样式区只去 CSS 块注释，
/// 谁也不越界。`strip_comments` 留给真正的 JS 区。
fn strip_css_comments(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            let mut j = i + 2;
            while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                j += 1;
            }
            out.push(' ');
            i = (j + 2).min(chars.len());
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 取出 `<open>…</close>` 这一段（Vue SFC 的模板区 / 样式区）。
fn section(html: &str, open: &str, close: &str) -> String {
    let start = html.find(open).unwrap_or(0);
    let rest = &html[start..];
    let end = rest.find(close).map(|n| n + close.len()).unwrap_or(rest.len());
    rest[..end].to_string()
}

fn strip_html_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '<' && chars.get(i + 1) == Some(&'!') && chars.get(i + 2) == Some(&'-') {
            let mut j = i + 3;
            while j + 2 < chars.len()
                && !(chars[j] == '-' && chars[j + 1] == '-' && chars[j + 2] == '>')
            {
                j += 1;
            }
            out.push(' ');
            i = (j + 3).min(chars.len());
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 每个 `<button>` 都必须有可访问名：可见文字，或 `aria-label` / `title`。
///
/// ## 为什么值得单列一条守卫
///
/// 全篇 91 个 `<button>` 里，图标按钮（内容只有一个 `<i class="ph …">`）
/// 占了一大片。Phosphor 图标是 CSS 背景，**没有任何文字内容** ——
/// 没有 `title`/`aria-label` 的图标按钮在屏幕阅读器里就只是「按钮」，
/// 读不出它是关闭、刷新还是删除。
///
/// 这类缺陷**完全静默**：界面看得见那个 ✕，测试也测得过去（点得动），
/// 只有用辅助技术的人才知道"这个按钮没名字"。
///
/// 踩过的坑：判据里**不能**把 `{{ … }}` 当噪声剥掉再判断"有没有文字"。
/// 第一版这么写，把 8 个按钮误报成缺名 —— 它们的标签其实是
/// `{{ isPairingSubmit ? '取消连接' : '取消' }}`，`{{ }}` 就是**可见文字**。
/// 剥掉插值等于凭空给一批按钮造了个假缺陷。
#[test]
fn every_button_must_have_an_accessible_name() {
    let vue = std::fs::read_to_string(ui_src().join("App.vue")).expect("读取 App.vue 失败");
    let src = strip_html_comments(&vue);
    let tpl_end = src.find("<script").unwrap_or(src.len());
    let tpl = &src[..tpl_end];

    let mut i = 0usize;
    let mut total = 0usize;
    let mut unnamed: Vec<String> = Vec::new();
    while let Some(rel) = tpl[i..].find("<button") {
        let start = i + rel;
        let Some(gt) = tpl[start..].find('>') else { break };
        // ⚠️ 必须走字节：`str` 不能用 `usize` 直接索引（本文件含中文，
        // 字符边界不等于字节边界）。`<` `/` `>` 都是 ASCII，按字节比安全。
        if tpl.as_bytes()[start + gt - 1] == b'/' {
            // 自闭合标签，跳过
            i = start + gt + 1;
            continue;
        }
        let open_tag = &tpl[start..start + gt + 1];
        // 配对 </button>，处理嵌套
        let mut depth = 1usize;
        let mut j = start + gt + 1;
        let mut close = None;
        while j < tpl.len() && depth > 0 {
            let no = tpl[j..].find("<button").map(|n| j + n);
            let nc = tpl[j..].find("</button>").map(|n| j + n);
            match (no, nc) {
                (_, None) => break,
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    j = o + 7;
                }
                (_, Some(c)) => {
                    depth -= 1;
                    j = c + 9;
                    if depth == 0 {
                        close = Some(c);
                    }
                }
            }
        }
        let Some(c) = close else { break };
        let inner = &tpl[start + gt + 1..c];
        total += 1;

        // 有没有名字：可见文字（含 {{ 插值 —— 它就是文字）或 aria-label / title
        let has_name_attr = open_tag.contains("aria-label=") || open_tag.contains("title=");
        // 去标签，但**保留** {{ ... }} 的内容
        let text_only = inner
            .split('<')
            .map(|chunk| match chunk.find('>') {
                Some(k) => &chunk[k + 1..],
                None => chunk,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let visible = !text_only.split_whitespace().collect::<Vec<_>>().is_empty();
        // 纯插值的按钮：内容是 "{{ … }}"，去掉标签后仍是 "{{ … }}"，
        // split_whitespace 非空 —— 已在上面覆盖。
        if !has_name_attr && !visible {
            let cls = class_tokens(open_tag).join(".");
            unnamed.push(format!(
                "<button class=\"{}\"> 内容只有图标，没有 title / aria-label / 文字",
                cls
            ));
        }
        i = c + 9;
    }

    assert!(
        total >= 80,
        "只数到 {} 个 <button> —— 配对逻辑可能失效，这条守卫会退化成空壳",
        total
    );
    assert!(
        unnamed.is_empty(),
        "以下按钮**没有可访问名**，屏幕阅读器只会读出「按钮」：\n  - {}",
        unnamed.join("\n  - ")
    );
}

/// 每个表单控件都必须有可访问名（四条途径任一即可）。
///
/// ## 为什么要单列一条守卫
///
/// 实测找到的两类真缺陷，都**完全静默**：控件能点、能用、界面也好看，
/// 只有用屏幕阅读器的人才知道"这一项叫什么"。
///
/// 1. **开关只有装饰文字**：`<label class="fluent-switch">` 里**嵌套**了
///    `<input type="checkbox">`，看着像有关联，可 label 内部只有一个
///    `<span class="fluent-slider"></span>` —— **一个文字都没有**；
///    真正的文案 `<strong>开机自启</strong>` 在 label **外面**。
///    结果就是「复选框，未选中」，用户不知道这一项是什么。
///
/// 2. **label 与控件是兄弟且没有 `for`**：`.form-input-group` 里的
///    `<label>对方 IP 地址</label>` 和 `<input>` 平级，`for` 也没有，
///    等于完全没关联。
///
/// ## 判据要点
///
/// 认全四条命名途径，否则会误报一堆：
/// `aria-label` / `aria-labelledby`（须指向存在的 id）/ `title` /
/// 被 label 关联（`for=id` 或**嵌套**且 label 内有文字）。
///
/// ⚠️ 找 `label[for]` 时必须排除 `v-for="m in scopeModes"` ——
/// `-` 与 `f` 之间存在 `\b`，`\bfor=` 会命中 `v-for=`。我第一版就栽在这，
/// 报出两个根本不存在的 `for="m in scopeModes"` 悬空引用。
#[test]
fn every_form_control_must_have_an_accessible_name() {
    let vue = std::fs::read_to_string(ui_src().join("App.vue")).expect("读取 App.vue 失败");
    let src = strip_html_comments(&vue);
    let tpl_end = src.find("<script").unwrap_or(src.len());
    let tpl = &src[..tpl_end];

    // 现有的 id
    let ids: std::collections::BTreeSet<&str> = tpl
        .match_indices("id=\"")
        .filter_map(|(i, _)| {
            let rest = &tpl[i + 4..];
            rest.find('"').map(|q| &rest[..q])
        })
        .collect();

    // label[for] -> 该 label 的原始属性串（排除 v-for）
    let mut label_for: std::collections::BTreeSet<&str> = Default::default();
    for (i, _) in tpl.match_indices("<label") {
        // 确认这是开标签而不是 <labeling 之类
        let after = tpl[i + 6..].chars().next();
        if !matches!(after, Some(c) if c.is_whitespace() || c == '>') {
            continue;
        }
        let Some(gt) = tpl[i..].find('>') else { continue };
        let attrs = &tpl[i + 6..i + gt];
        let bytes = tpl.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = attrs[from..].find("for=\"") {
            let at = i + 6 + from + rel;
            // 关键：前面不能是 '-'，否则是 v-for
            let preceded_by_dash = at > 0 && bytes[at - 1] == b'-';
            if !preceded_by_dash {
                if let Some(q) = tpl[at + 5..].find('"') {
                    label_for.insert(&tpl[at + 5..at + 5 + q]);
                }
            }
            from = from + rel + 5;
        }
    }

    // 遍历模板，维护"当前所在的 <label> 栈"，并记录每个 label **有没有文字**。
    //
    // ⚠️ 两个坑，都踩过：
    //
    // 1. `</label>` **不会**被 `find("<label")` 命中 —— 中间那个 `/`
    //    让两个字符串对不上。我第一版只找开标签，label_depth 只增不减，
    //    一旦遇到第一个 <label> 就永远为真，后面所有控件都被当成
    //    "嵌套在 label 里"而被放行 —— 守卫静默退化成空壳，
    //    而 `total >= 15` 这个反空壳断言照样通过（它数的是控件，不是深度）。
    //
    // 2. **嵌套不等于有名字**。`.fluent-switch` 的 label 里只有
    //    `<span class="fluent-slider"></span>` —— 零文字。
    //    按名字算，label 的名字是空的，控件照样无名。
    //    这正是本轮修掉的三个开关的缺陷，我却一度用 `depth > 0` 放行了它们。
    let mut label_stack: Vec<bool> = Vec::new(); // true = 该 label 内有可见文字
    let mut cursor = 0usize;
    let mut total = 0usize;
    let mut max_depth = 0usize;
    let mut unnamed: Vec<String> = Vec::new();
    loop {
        let open = tpl[cursor..].find("<label").map(|n| cursor + n);
        let close = tpl[cursor..].find("</label>").map(|n| cursor + n);
        let control = ["<input", "<select", "<textarea"]
            .iter()
            .filter_map(|t| tpl[cursor..].find(t).map(|n| cursor + n))
            .min();
        let pos = [open, close, control].into_iter().flatten().min();
        let Some(pos) = pos else { break };

        if pos == open.unwrap_or(usize::MAX) {
            // 这个 label 的开标签到它的 </label> 之间有没有可见文字？
            let body_end = tpl[pos..].find("</label>").map(|n| pos + n).unwrap_or(tpl.len());
            let body = &tpl[pos..body_end];
            let has_text = {
                // 去标签、也去插值外壳，但**保留**插值内容（{{ }} 就是可见文字）
                let mut s2 = String::new();
                let mut ch = body.chars().peekable();
                while let Some(c) = ch.next() {
                    if c == '<' {
                        for c2 in ch.by_ref() {
                            if c2 == '>' {
                                break;
                            }
                        }
                    } else if c == '{' {
                        s2.push('{');
                    } else {
                        s2.push(c);
                    }
                }
                !s2.split_whitespace().collect::<Vec<_>>().is_empty()
            };
            label_stack.push(has_text);
            max_depth = max_depth.max(label_stack.len());
            cursor = pos + 6;
        } else if pos == close.unwrap_or(usize::MAX) {
            label_stack.pop();
            cursor = pos + 7;
        } else {
            let c = pos;
            let Some(gt) = tpl[c..].find('>') else { break };
            let attrs = &tpl[c..c + gt];
            total += 1;
            let labelled_by_ancestor = label_stack.iter().any(|t| *t);
            let named = attrs.contains("aria-label=")
                || attrs
                    .match_indices("aria-labelledby=\"")
                    .next()
                    .and_then(|(i, _)| {
                        let rest = &attrs[i + "aria-labelledby=\"".len()..];
                        rest.find('"').map(|q| &rest[..q])
                    })
                    .map(|v| v.split_whitespace().all(|x| ids.contains(x)))
                    .unwrap_or(false)
                || attrs.contains("title=")
                || attrs
                    .match_indices("id=\"")
                    .next()
                    .and_then(|(i, _)| {
                        let rest = &attrs[i + 4..];
                        rest.find('"').map(|q| &rest[..q])
                    })
                    .map(|id| label_for.contains(id))
                    .unwrap_or(false)
                || labelled_by_ancestor;
            if !named {
                unnamed.push(format!(
                    "<{} {}",
                    tpl[c + 1..].split_whitespace().next().unwrap_or("?"),
                    attrs.split_whitespace().collect::<Vec<_>>().join(" ")
                ));
            }
            cursor = c + gt + 1;
        }
    }

    // 反空壳的另一半：深度必须真的涨过又落回来。
    // 只断言 total 是不够的 —— 上面那个"只增不减"的 bug 就能混过去。
    assert!(
        max_depth >= 1 && label_stack.is_empty(),
        "label 嵌套深度异常：峰值 {}、结束时剩 {} 层 —— 应当有涨有落。\
         若结束时仍非空，说明 </label> 没被识别（`find(\"<label\")` 命不中闭标签）",
        max_depth,
        label_stack.len()
    );

    assert!(
        total >= 15,
        "只数到 {} 个表单控件 —— 遍历逻辑可能失效，这条守卫会退化成空壳",
        total
    );
    assert!(
        unnamed.is_empty(),
        "以下表单控件**没有任何可访问名**，屏幕阅读器只会读出控件类型：\n  - {}\n\
         （aria-label / aria-labelledby / title / label 的 for 或嵌套，四者都没有）",
        unnamed.join("\n  - ")
    );
}

/// 表单控件的开闭标签必须自洽：`<select>` 得配 `</select>`，`<input>` 必须自闭合。
///
/// ## 为什么值得单列一条守卫
///
/// 本轮我自己踩了这个坑，而且是**批量**踩的：写补丁时用循环变量
/// `for tag in ("input", "select")` 去重建标签，而不是用真正匹配到的
/// `tm.group(1)`。结果 5 个设置页 `<select>` 被改成 `<input>`
/// （闭合标签仍是 `</select>`），配对码 `<input>` 被改成 `<select>` ——
/// **文本框变成了下拉框**，选择框直接消失。
///
/// 这种破坏**能通过构建**（Vue 不会因为标签配错而报错），
/// 也不会让任何无障碍守卫报警（控件照样有名字）——
/// 只有肉眼或真机操作才看得出来。属于"改坏界面但没人拦"的典型。
#[test]
fn form_controls_must_be_tag_balanced() {
    let vue = std::fs::read_to_string(ui_src().join("App.vue")).expect("读取 App.vue 失败");
    let src = strip_html_comments(&vue);
    let tpl_end = src.find("<script").unwrap_or(src.len());
    let tpl = &src[..tpl_end];

    let mut opens = 0usize;
    let mut closes = 0usize;
    let mut bad: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = tpl[cursor..].find("<select") {
        let at = cursor + rel;
        let Some(gt) = tpl[at..].find('>') else { break };
        let attrs = &tpl[at + 7..at + gt];
        if attrs.trim_end().ends_with('/') {
            bad.push("`<select … />` 自闭合了 —— select 不能自闭合".to_string());
        } else {
            opens += 1;
        }
        // 往回看：这一段必须以 </select> 收尾
        let seg_end = tpl[at..].find("</select>").map(|n| at + n + 9);
        let next_open = tpl[at + gt..].find("<select").map(|n| at + gt + n);
        if let (Some(se), Some(no)) = (seg_end, next_open) {
            if no < se {
                bad.push(format!(
                    "<select …> 与 </select> 之间又开了新的 <select>（L{}）—— 开闭不配对",
                    tpl[..no].matches('\n').count() + 1
                ));
            }
        }
        cursor = at + 7;
    }
    for _ in tpl.match_indices("</select>") {
        closes += 1;
    }
    assert_eq!(
        opens, closes,
        "<select> 开标签 {} 个、</select> {} 个，数量对不上 —— \
         很可能某个 select 被误改成了别的标签",
        opens, closes
    );

    // <input> 必须自闭合：input 是 void 元素
    cursor = 0;
    let mut inputs = 0usize;
    while let Some(rel) = tpl[cursor..].find("<input") {
        let at = cursor + rel;
        let Some(gt) = tpl[at..].find('>') else { break };
        inputs += 1;
        let attrs = tpl[at + 6..at + gt].trim_end();
        if !attrs.ends_with('/') {
            // 允许写成 <input … /> 之外的形态吗？不允许：input 是 void
            bad.push(format!(
                "L{} 的 <input> 没有自闭合（属性结尾是 {:?}）",
                tpl[..at].matches('\n').count() + 1,
                &attrs[attrs.len().saturating_sub(18)..]
            ));
        }
        cursor = at + 6;
    }

    assert!(
        inputs >= 8,
        "只数到 {} 个 <input> —— 扫描逻辑可能失效",
        inputs
    );
    assert!(
        bad.is_empty(),
        "表单控件标签不配对：\n  - {}",
        bad.join("\n  - ")
    );
}

/// 扫描模板里的标题标签，返回 `(标签, 原始属性, 父元素的 class 列表)`，
/// **按出现顺序**排列。
///
/// ## 为什么不能直接用 `scan_open_tags`
///
/// `scan_open_tags` 找标签结尾 `>` 时要求它后面是空白 / `<` / `{`，
/// 遇到另一个 `<` 就 break。目的是躲开 `:class="{ a: b > c }"` 这类
/// 插值里的 `>`。但代价是：**凡是紧跟文字的标签它一个都看不见** ——
///
/// ```html
/// <h2>传输记录</h2>
/// ^^^^^^^ 这个 `>` 后面是「传」，既不是空白也不是 `<`
/// ```
///
/// 本文件里所有标题都是这个形状，所以 `scan_open_tags` 扫出 0 个标题
/// （实测断言失败：`实得 0 个`）。
///
/// 这里改成**跟引号状态**：属性值里的 `>` 不算结尾，插值里的 `>` 同样
/// 被引号包住，于是既能正确识别 `<h2>文字</h2>`，又不会被
/// `:class="{ active: cur !== 'settings' }"` 里的 `>` 带偏。
///
/// ## 为什么要**父元素的 class**
///
/// CSS 里 `.header-titles h2` 是后代选择器：类名在父级
/// `<div class="header-titles">` 上，标签在子级 `<h2>` 上。
/// 只看"同一个元素上有没有 class 和 h2"永远匹配不上 ——
/// 这是本轮第一次写法的错误（一条都没扫出来，靠 `checked >= 5` 的
/// 反空壳断言兜住）。
fn scan_headings_with_parents(tpl: &str) -> Vec<(String, String, Vec<String>)> {
    const VOID: [&str; 6] = ["br", "img", "input", "hr", "meta", "link"];
    let chars: Vec<char> = tpl.chars().collect();
    let mut out = Vec::new();
    // (标签名, 该标签的 class 列表)
    let mut stack: Vec<(String, Vec<String>)> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '<' {
            i += 1;
            continue;
        }
        // HTML 注释：整段跳过，里面的标签不算
        if chars.get(i + 1) == Some(&'!') {
            let mut j = i + 2;
            while j + 2 < chars.len()
                && !(chars[j] == '-' && chars[j + 1] == '-' && chars[j + 2] == '>')
            {
                j += 1;
            }
            i = j + 3;
            continue;
        }
        let closing = chars.get(i + 1) == Some(&'/');
        let mut j = if closing { i + 2 } else { i + 1 };
        let mut name = String::new();
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '-' || chars[j] == '_') {
            name.push(chars[j]);
            j += 1;
        }
        if name.is_empty() {
            i += 1;
            continue;
        }
        // 引号感知的属性扫描：属性值里的 '>' 不是标签结尾
        let mut k = j;
        let mut quote: Option<char> = None;
        let mut end = None;
        while k < chars.len() {
            let c = chars[k];
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                }
                None => {
                    if c == '"' || c == '\'' {
                        quote = Some(c);
                    } else if c == '>' {
                        end = Some(k);
                        break;
                    } else if c == '<' {
                        break;
                    }
                }
            }
            k += 1;
        }
        let Some(e) = end else {
            i = j.max(i + 1);
            continue;
        };
        let attrs: String = chars[j..e].iter().collect();
        let self_close = attrs.trim_end().ends_with('/');
        if closing {
            if let Some(pos) = stack.iter().rposition(|(n, _)| *n == name) {
                stack.truncate(pos);
            }
        } else {
            let is_heading = name.len() == 2
                && name.starts_with('h')
                && name[1..].chars().all(|c| c.is_ascii_digit());
            if is_heading {
                let parents = stack.last().map(|(_, c)| c.clone()).unwrap_or_default();
                out.push((name.clone(), attrs.clone(), parents));
            }
            if !self_close && !VOID.contains(&name.as_str()) {
                stack.push((name.clone(), class_tokens(&attrs)));
            }
        }
        i = e + 1;
    }
    out
}

/// 扫描 CSS，返回选择器里出现的「(.class, hN)」组合，附上原始选择器。
fn css_heading_pairs(css: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = css.chars().collect();
    let mut sel_start = 0usize;
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '{' => {
                if depth == 0 {
                    let raw: String = chars[sel_start..i].iter().collect();
                    let selector = raw.rsplit('}').next().unwrap_or("").trim().to_string();
                    for sel in selector.split(',') {
                        let mut classes: Vec<String> = Vec::new();
                        let mut tags: Vec<String> = Vec::new();
                        for tok in sel.split(|c: char| {
                            c.is_whitespace() || c == '>' || c == '+' || c == '~'
                        }) {
                            if tok.is_empty() {
                                continue;
                            }
                            if let Some(c) = tok.strip_prefix('.') {
                                classes.push(c.to_string());
                            } else if tok.len() == 2
                                && tok.starts_with('h')
                                && tok[1..].chars().all(|c| c.is_ascii_digit())
                            {
                                tags.push(tok.to_string());
                            }
                        }
                        for c in &classes {
                            for t in &tags {
                                out.push((c.clone(), t.clone(), sel.trim().to_string()));
                            }
                        }
                    }
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth <= 0 {
                    sel_start = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// CSS 里凡是「类名 + 标题标签」的后代选择器，标签必须与模板里的实际标签一致。
///
/// ## 为什么值得单列一条守卫
///
/// 这是本轮改动里最隐蔽的一种回归：把 `h3` 改成 `h2` 之后，
/// CSS 里的 `.header-titles h3` **不会报错**，只是从此不匹配任何元素，
/// 16px 字号静默退回浏览器默认的 1.5em —— 界面只是"字突然变大了"，
/// 既不崩溃也不告警，肉眼极易漏掉（而且往往正好是没人盯着的那一屏）。
///
/// 这里做的是**通用**检查：把 CSS 里所有「.class hN」后代选择器抽出来，
/// 逐个核对模板里是否真的存在「class 为 A 的祖先内部有 <hN>」。
/// 以后再改任何标题标签，这里都会立刻报警。
///
/// 不引 `regex` 依赖 —— 为一条测试守卫给桌面端加正则库不划算。
#[test]
fn css_tag_selectors_must_match_the_actual_heading_tags() {
    let vue = std::fs::read_to_string(ui_src().join("App.vue")).expect("读取 App.vue 失败");
    // 分区扫描，各去各的注释（理由见 strip_css_comments 的注释）
    let clean = strip_html_comments(&vue);
    let tpl = {
        let end = clean.find("<script").unwrap_or(clean.len());
        &clean[..end]
    };
    let css = strip_css_comments(&section(&clean, "<style", "</style>"));

    let headings = scan_headings_with_parents(tpl);
    let wanted = css_heading_pairs(&css);

    // 反空壳：两条抽取都得真的抓到东西，否则这条守卫等于没写
    assert!(
        headings.len() >= 15,
        "模板里只解析出 {} 个标题 —— 标题扫描可能失效（`scan_open_tags` 就扫不到 \
         紧跟文字的标签，见它的注释）",
        headings.len()
    );
    assert!(
        wanted.len() >= 4,
        "CSS 里只解析出 {} 条「.class hN」选择器 —— 选择器扫描可能失效，\
         这条守卫会退化成空壳",
        wanted.len()
    );

    // 方向一：CSS 说 `.A hN`，模板里得真有这样一对，否则规则空转。
    for (cls, tag, sel) in &wanted {
        let found = headings
            .iter()
            .any(|(t, _, parents)| *t == *tag && parents.iter().any(|p| p == cls));
        assert!(
            found,
            "CSS 选择器 `{}` 要求「class 含 `{}` 的元素内部有一个 <{}>」，\
             但模板里没有这样的元素 —— 标签改过而选择器没跟着改，\
             这条规则会**静默失效**，字号退回浏览器默认值",
            sel,
            cls,
            tag
        );
    }

    // 方向二（这一半才是关键）：CSS 的 `.A hN` 会作用于 A 里的**每一个**
    // hN，所以只要 A 底下还有别的标签的标题，它就是**没被样式接住**的。
    //
    // ⚠️ 只做方向一会漏：`.panel-section-divider h3` 对应 7 个区块，
    // 哪怕把其中 1 个的 h3 改成 h2，方向一依然成立（还剩 6 个 h3），
    // 而那一个区块的字号会静默退回浏览器默认值。
    // 这是"检查存在性"和"检查完整性"的区别，前者太弱。
    for (tag, _, parents) in &headings {
        for p in parents {
            let styled: Vec<&String> = wanted
                .iter()
                .filter(|(cls, _, _)| cls == p)
                .map(|(_, t, _)| t)
                .collect();
            if styled.is_empty() {
                continue;
            }
            assert!(
                styled.contains(&tag),
                "class `{}` 的标题里有一个是 <{}>，而 CSS 只写了 `.{} {}` \
                 —— 这个标题**没有对应的样式规则**，字号会静默退回浏览器默认值。\
                 改标签时必须同步改选择器",
                p,
                tag,
                p,
                styled
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            );
        }
    }
}
/// 扫描 Vue 模板里的开标签, 返回 `(行号, 标签名, 原始属性文本)`。
///
/// ## 为什么不能用 `>` 当标签结束符
///
/// 早先的版本按"从 `<` 到下一个 `>`"截取属性文本, 在真实模板上
/// **一个标签都扫不到**: 属性是跨行写的, 而属性值里本来就可以合法
/// 出现 `>` —— 典型是 Vue 的比较表达式:
///
/// ```html
/// <div :class="{ active: cur !== 'settings' }"
/// ```
///
/// 这里的 `>` 让扫描在标签**刚开始**就截断, 于是断言拿到的是半截
/// 属性, 守卫跟着变成"永远通过"的空壳。
///
/// 空壳守卫比没有守卫更糟: 绿灯让人以为这条不变量有人守着, 于是
/// 下一个人放心地删掉 `tabindex` —— 直到有键盘用户报障才被发现。
///
/// ## 现在的判据
///
/// 标签以 `/>` 或 `>` **且其后紧跟的行尾/空白**结束。为什么加
/// "其后必须是行尾"这一条: `:class="{ a: x > y }"` 里的 `>` 后面
/// 跟着 ` y`, 不是行尾; 而真正的标签结束 `>` 后面要么是换行、要么
/// 是 `/>`。这条判据不需要完整的 HTML 解析器, 却足以避开最常见的
/// 误截断。
///
/// 全程在 `Vec<char>` 上索引, 混用字符下标与 `&str` 字节偏移会在
/// 遇到中文时 panic 在 "not a char boundary"。
fn scan_open_tags(src: &str) -> Vec<(usize, String, String)> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] != '<' {
            i += 1;
            continue;
        }
        // `<!--` 注释与 `</` 闭合标签都跳过
        if chars.get(i + 1) == Some(&'/') || chars.get(i + 1) == Some(&'!') {
            i += 1;
            continue;
        }
        // 读标签名
        let mut j = i + 1;
        let mut name = String::new();
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '-') {
            name.push(chars[j]);
            j += 1;
        }
        if name.is_empty() {
            i += 1;
            continue;
        }

        // 找标签结束: `>` 且其后是行尾/空白/`<`/`{`
        let mut k = j;
        let mut end = None;
        while k < chars.len() {
            if chars[k] == '>' {
                let next = chars.get(k + 1);
                let is_tag_end = match next {
                    None => true,
                    Some(&c) => c.is_whitespace() || c == '<' || c == '{',
                };
                if is_tag_end {
                    end = Some(k);
                    break;
                }
            }
            // 撞上另一个 `<` 说明这个标签没写完, 放弃避免误吞
            if chars[k] == '<' {
                break;
            }
            k += 1;
        }
        let Some(end) = end else {
            i = j.max(i + 1);
            continue;
        };

        let attrs: String = chars[j..end].iter().collect();
        let line_no = chars[..i].iter().filter(|&&c| c == '\n').count() + 1;
        out.push((line_no, name, attrs));
        i = end + 1;
    }
    out
}

/// 从属性文本里取出 class 的**各个词元**。
///
/// 不能用 `contains("modal-dialog")`: 那样会连带命中
/// `.modal-dialog-titlebar` —— 那是弹窗**内部**的标题行, 是普通布局
/// 元素, 给它加 `role="dialog"` 反而是错的。一条会误报的守卫比没有
/// 守卫更贵: 人会开始习惯性忽略它的输出。
fn class_tokens(attrs: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = attrs;
    while let Some(idx) = rest.find("class=") {
        let after = &rest[idx + "class=".len()..];
        let after = after.trim_start_matches(['"', '\'']);
        let end = after
            .find(|c: char| c == '"' || c == '\'' || c.is_whitespace() || c == '>')
            .unwrap_or(after.len());
        out.extend(after[..end].split_whitespace().map(str::to_string));
        rest = &after[end..];
    }
    out
}

/// 每个弹窗都必须声明 `role="dialog"` + `aria-modal` + 可聚焦。
///
/// ## 为什么值得单列一条守卫
///
/// 弹窗的语义缺失是**沉默**的: 没有 `role` 时屏幕阅读器只是把它
/// 当成一堆普通文本, 不会念"对话框"; 没有 `aria-modal` 时它甚至
/// 会继续朗读背后的界面内容。用户听到的是一段没有上下文的朗读,
/// 却完全不知道自己在弹窗里。
///
/// 更关键的是 `tabindex="-1"` —— 焦点陷阱依赖弹窗容器本身可编程
/// 聚焦。没有它, 打开一个"只有文本没有控件"的弹窗时, 焦点无处可去,
/// Tab 会直接跳出到背后被遮住的界面。
#[test]
fn every_modal_must_declare_dialog_semantics() {
    let files = source_files(&ui_src());
    let mut failures: Vec<String> = Vec::new();
    let mut modal_count = 0usize;

    for f in &files {
        if f.extension().and_then(|x| x.to_str()) != Some("vue") {
            continue;
        }
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(f).unwrap_or_default();
        let src = strip_comments(&raw);

        for (line_no, tag, attrs) in scan_open_tags(&src) {
            if tag != "div" {
                continue;
            }
            let is_container = class_tokens(&attrs)
                .iter()
                .any(|c| c == "fluent-modal-dialog" || c == "code-prompt-modal");
            if !is_container {
                continue;
            }

            modal_count += 1;
            let has_role = attrs.contains("role=\"dialog\"") || attrs.contains("role=\"alertdialog\"");
            let has_modal = attrs.contains("aria-modal=\"true\"");
            let has_focusable = attrs.contains("tabindex=\"-1\"");
            let has_directive = attrs.contains("v-modal-focus");

            let mut missing = Vec::new();
            if !has_role {
                missing.push("role");
            }
            if !has_modal {
                missing.push("aria-modal");
            }
            if !has_focusable {
                missing.push("tabindex=\"-1\"");
            }
            if !has_directive {
                missing.push("v-modal-focus");
            }
            if !missing.is_empty() {
                failures.push(format!(
                    "{}:{} 弹窗缺少 {}",
                    name,
                    line_no,
                    missing.join(" / ")
                ));
            }
        }
    }

    assert!(
        modal_count > 0,
        "一个弹窗容器都没找到 —— 守卫本身可能已失效 (模板结构变了?)"
    );
    assert!(
        failures.is_empty(),
        "以下弹窗缺少对话框语义 (屏幕阅读器会当普通文本读, 焦点也会跑到背后):\n  {}",
        failures.join("\n  ")
    );
}

/// 可点击的 `<div>` / `<span>` 必须键盘可达。
///
/// ## 为什么值得单列一条守卫
///
/// `<div @click>` 默认**不可聚焦**: 键盘用户 Tab 不到, 按 Enter 也没
/// 反应。设备行、本机卡片、「其他设备」折叠标题、保存目录路径、
/// 穿梭面板的文件行都是这种形态, 而"选中设备 / 勾选要发的文件"正是
/// 这个应用的核心动作 —— 不是边角装饰。
///
/// ## 两类刻意的豁免
///
/// - **`@click.self`**: 遮罩层的"点空白处关闭"。它不是一个**点击目标**
///   —— 用户不会瞄准它按下, 它只是背景。键盘用户有 Escape 兜底
///   (见 `handleDialogKeydown`), 把遮罩塞进 Tab 序列反而是噪声。
/// - **`<button>` / `<a href>`**: 天然可聚焦, 不该被这条守卫误伤。
#[test]
fn clickable_non_button_elements_must_be_keyboard_reachable() {
    let files = source_files(&ui_src());
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for f in &files {
        if f.extension().and_then(|x| x.to_str()) != Some("vue") {
            continue;
        }
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(f).unwrap_or_default();
        let src = strip_comments(&raw);

        for (line_no, tag, attrs) in scan_open_tags(&src) {
            if tag != "div" && tag != "span" {
                continue;
            }
            if !attrs.contains("@click") {
                continue;
            }
            // `.self` 修饰 = 只在点到元素自身时触发 = 遮罩关闭, 非点击目标
            if attrs.contains("@click.self=") {
                continue;
            }
            checked += 1;
            let has_tabindex = attrs.contains("tabindex=");
            let has_role = attrs.contains("role=");
            let has_directive = attrs.contains("v-key-activate");
            // 三者缺一不可, 而不是"或"的关系:
            //  - `role`  决定屏幕阅读器把它念成按钮而不是"一堆文本"
            //  - `tabindex` 决定 Tab 能不能停在它上面
            //  - `v-key-activate` 决定 Enter/Space 会不会真的触发点击
            //
            // 曾经写成 `tabindex && (role || directive)`, 结果"只留
            // role + tabindex、删掉 directive"这种改法能通过守卫 ——
            // 而那样元素**能聚焦但按 Enter 没反应**, 正是这条守卫要
            // 拦的那类半成品。
            let mut missing = Vec::new();
            if !has_role {
                missing.push("role");
            }
            if !has_tabindex {
                missing.push("tabindex");
            }
            if !has_directive {
                missing.push("v-key-activate");
            }
            if !missing.is_empty() {
                failures.push(format!(
                    "{}:{} <{} @click> 键盘不可达 (缺 {})",
                    name,
                    line_no,
                    tag,
                    missing.join(" + ")
                ));
            }
        }
    }

    // 守卫自身的有效性: 一个可点击 div/span 都扫不到, 说明扫描逻辑
    // 坏了 (多半是模板结构变了), 那时的"通过"毫无意义。
    assert!(
        checked > 0,
        "一个带 @click 的 div/span 都没扫到 —— 扫描器可能已失效"
    );
    assert!(
        failures.is_empty(),
        "以下可点击元素键盘不可达 (Tab 走不到, Enter 无反应):\n  {}",
        failures.join("\n  ")
    );
}

/// 异步列表必须有"加载中"或"空"的可见表达, 不能只留一片空白。
///
/// ## 为什么值得单列一条守卫
///
/// 穿梭面板的两个文件列表曾经**什么都不渲染**: `v-for` 在数组为空时
/// 不产生任何节点, 于是列目录的那几百毫秒里面板是纯空白。用户看到
/// 空白会以为"这个目录没文件"或"程序卡住了", 于是去点别的地方 ——
/// 空白传达的信息量是零, 而"正在加载"至少说明程序还活着。
///
/// 关键在于这类缺失**不产生任何错误**: 编译通过、运行时无异常、
/// 鼠标用户可能根本没注意到（列表回来得很快）。只有"我知道它该出现
/// 东西却没有"这种视角才看得见。
#[test]
fn async_lists_must_declare_a_loading_or_empty_state() {
    let files = source_files(&ui_src());
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    // 滚动列表容器的类名 -> 该类名应配套的"加载中/空"类名
    //
    // ⚠️ 右边必须是**完整 class 名**, 不能写 `skeleton-stack` 这种
    // 前缀/后缀片段: 真实类名是 `shuttle-skeleton-stack`, 而下面按
    // 词元边界匹配时, `skeleton-stack` 前面那个字符是 `-`(不是引号
    // 或空白), 于是**永远匹配不上** —— 守卫会把正确写法判成缺失,
    // 然后被人调宽成子串匹配, 又变回空壳。
    let containers = [
        ("shuttle-items-scroll", vec!["shuttle-skeleton-stack", "shuttle-empty-tip"]),
        ("staged-scroll-rows", vec!["staged-row"]),
    ];

    for f in &files {
        if f.extension().and_then(|x| x.to_str()) != Some("vue") {
            continue;
        }
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(f).unwrap_or_default();
        let src = strip_comments(&raw);

        for (container, companions) in &containers {
            // 找每个以该类名开头的开标签, 取其结束 `>`
            let needle = format!("class=\"{container}");
            let mut from = 0usize;
            while let Some(rel) = src[from..].find(&needle) {
                let at = from + rel;
                from = at + needle.len();
                let Some(rel_end) = src[at..].find('>') else {
                    break;
                };
                checked += 1;

                // 这个容器的"管辖范围" = 它之后 1200 字符
                // (骨架在子树内, 空状态提示是**平级**兄弟节点)。
                //
                // 为什么允许看兄弟: 空状态提示(`.shuttle-empty-tip`)是
                // 与滚动容器**平级**的, 不在子树里 —— 要求"必须在子树内"
                // 会把正确写法判成缺失, 那样的守卫只会被调宽而失去意义。
                let after = at + rel_end;
                let window_end = (after + 1200).min(src.len());
                if !src[after..].is_char_boundary(after)
                    || !src[after..].is_char_boundary(window_end)
                {
                    continue;
                }
                let scope = &src[after..window_end];

                // 必须按**完整 class 词元**匹配, 不能用 `contains`。
                //
                // 子串匹配会让这条守卫变成空壳: 把 `shuttle-skeleton-stack`
                // 改名成 `shuttle-skeleton-stack-removed`, `contains("skeleton-stack")`
                // 照样为真 —— 也就是"骨架被删掉了"这种情况**检测不到**。
                // 词元边界用引号/空白界定, 因为 class 值就是这么分隔的。
                let has_affordance = companions.iter().any(|c| {
                    scope.match_indices(c).any(|(idx, _)| {
                        let before = scope[..idx].chars().next_back();
                        let after_c = scope[idx + c.len()..].chars().next();
                        let boundary_before =
                            before.map_or(true, |ch| ch.is_whitespace() || ch == '"' || ch == '\'');
                        let boundary_after = after_c
                            .map_or(true, |ch| ch.is_whitespace() || ch == '"' || ch == '\'');
                        boundary_before && boundary_after
                    })
                });
                if !has_affordance {
                    let line_no = src[..at].matches('\n').count() + 1;
                    failures.push(format!(
                        "{}:{} .{} 里只有 v-for, 没有任何骨架/空状态节点 —— \
                         加载期间会是一片空白",
                        name, line_no, container
                    ));
                }
            }
        }
    }

    assert!(
        checked > 0,
        "一个滚动列表容器都没扫到 —— 扫描器可能已失效 (模板结构变了?)"
    );
    assert!(
        failures.is_empty(),
        "以下异步列表没有加载中/空状态 (加载期间表现为一片空白):\n  {}",
        failures.join("\n  ")
    );
}

/// 层级刻度必须只用令牌, 且**大小关系**符合语义。
///
/// ## 为什么必须查大小关系而不只是查"用了令牌"
///
/// 令牌化只保证"大家都用同一套名字", 不保证**顺序对**。
/// 真正的故障是: 叠加弹窗的 z-index 比它的父遮罩**低** ——
/// 于是"配对框里再弹出的输入匹配码"被压在配对框**下面**,
/// 用户看不见它, 只能以为程序卡住了。
///
/// 这类错误在"只用令牌"的前提下完全可以发生: 有人为了"看起来
/// 没那么大"把 nested 改成 200, 令牌化守卫一声不响。
#[test]
fn z_index_scale_must_use_tokens_and_respect_semantics() {
    let css = std::fs::read_to_string(ui_src().join("style.css")).expect("读取 style.css 失败");

    let read_tokens = |block: &str| -> Vec<(String, i64)> {
        block
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                if !t.starts_with("--z-") {
                    return None;
                }
                let (name, value) = t.split_once(':')?;
                value
                    .trim()
                    .trim_end_matches(';')
                    .parse::<i64>()
                    .ok()
                    .map(|v| (name.trim().to_string(), v))
            })
            .collect()
    };

    let light_at = css
        .find("[data-theme=\"light\"]")
        .expect("缺少浅色主题块");
    let dark = read_tokens(&css[..light_at]);
    let light = read_tokens(&css[light_at..]);

    assert!(!dark.is_empty(), "style.css 里没有 --z-* 层级令牌");
    assert_eq!(
        dark.len(),
        light.len(),
        "两套主题的 --z-* 令牌数量不一致 (深色 {} / 浅色 {}) —— \
         浅色下未定义的 var() 会让声明**直接失效**",
        dark.len(),
        light.len()
    );

    // 语义约束: (必须更大者, 必须更小者, 为什么)
    let constraints: Vec<(&str, &str, &str)> = vec![
        ("--z-titlebar-caption", "--z-titlebar", "caption 按钮要在标题栏之上"),
        ("--z-overlay", "--z-titlebar-caption", "弹窗遮罩要在标题栏之上"),
        ("--z-overlay-nested", "--z-overlay", "叠加弹窗要在父遮罩之上, 否则被压住看不见"),
        ("--z-toast", "--z-overlay", "提示要在弹窗之上可见"),
        ("--z-toast", "--z-overlay-nested", "提示要在叠加弹窗之上可见"),
    ];

    let lookup = |toks: &[(String, i64)], name: &str| -> Option<i64> {
        toks.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    };

    let mut failures: Vec<String> = Vec::new();
    for (theme, toks) in [("深色", &dark), ("浅色", &light)] {
        for (hi, lo, why) in &constraints {
            match (lookup(toks, hi), lookup(toks, lo)) {
                (Some(h), Some(l)) => {
                    if h <= l {
                        failures.push(format!(
                            "{} · {}: {}={} 未大于 {}={} —— {}",
                            theme, why, hi, h, lo, l, why
                        ));
                    }
                }
                _ => failures.push(format!("{} · 缺少令牌 {} 或 {}", theme, hi, lo)),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "层级刻度的大小关系错了 (叠加层被压住是最典型的症状):\n  {}",
        failures.join("\n  ")
    );

    // 组件里不许再出现裸数字
    for f in source_files(&ui_src()) {
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(&f).unwrap_or_default();
        for (i, line) in strip_comments(&raw).lines().enumerate() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("z-index:") {
                let v = rest.trim().trim_end_matches(';').trim();
                if !v.starts_with("var(") && !v.is_empty() && v != "auto" {
                    panic!(
                        "{}:{} 用了裸 z-index: {} —— 必须走 --z-* 令牌, \
                         否则大小关系无从检查 (裸数字让『谁盖住谁』只存在于某个人脑子里)",
                        name,
                        i + 1,
                        v
                    );
                }
            }
        }
    }
}

/// 进度条必须用 `transform: scaleX()` 而不是动画 `width`。
///
/// ## 为什么值得单列一条守卫
///
/// `width` 是**布局属性**: 每一帧都要重排。core 每 4MB 分片发一次
/// 进度事件 (`protocol.rs::CHUNK_SIZE`), 按 110MB/s 算就是**每秒
/// 约 27 次** —— 传输大文件时进度条本来就在持续重排整个文档。
///
/// 这类性能问题的外观极具迷惑性: 机器不慢、网络不慢、功能全对,
/// 只是界面"偶尔顿一下"。归因时几乎不可能想到是那条 4px 高的
/// 进度条。
#[test]
fn progress_bars_must_not_animate_layout_properties() {
    let files = source_files(&ui_src());
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for f in &files {
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(&f).unwrap_or_default();
        let src = strip_comments(&raw);

        // 收集 `*-fill` 之类"被进度驱动的填充条"规则
        for cls in [
            "speed-dock-fill",
            "update-progress-fill",
            "pin-countdown-fill",
        ] {
            // 只取**裸类名**规则(`.speed-dock-fill {`), 不取
            // `.desktop-speed-dock.failed .speed-dock-fill {` 这种带前置
            // 选择器的修饰规则 —— 后者只改颜色, 判它"缺 transform-origin"
            // 是误报。
            let needle = format!("\n.{} {{", cls);
            let Some(rel) = src.find(&needle) else {
                continue;
            };
            let at = rel + 1;
            let brace = needle.len() - 2;
            let Some(close_rel) = src[at + brace..].find('}') else {
                continue;
            };
            let rule = &src[at..at + brace + close_rel];
            checked += 1;

            // transition 的**取值**里不能出现 width/height/top/left。
            // 只看"规则里有没有 width"会把静态的 `width: 100%` 也算进去
            // —— 而那正是新实现需要的那一行(width 固定, 用 scaleX 改变比例)。
            for prop in ["width", "height", "top", "left"] {
                if src[at + brace..at + brace + close_rel].contains(&format!("{}:", prop))
                    && rule.contains(&format!("transition:"))
                    && rule
                        .lines()
                        .any(|l| l.trim_start().starts_with("transition:") && l.contains(prop))
                {
                    failures.push(format!(
                        "{} .{} 的 transition 动画了 {} —— 布局属性会逐帧重排, \
                         改用 transform: scaleX()",
                        name, cls, prop
                    ));
                }
            }
            if !rule.contains("transform-origin") {
                failures.push(format!(
                    "{} .{} 缺 transform-origin: left —— scaleX 默认从中心撑开, \
                     与 width 增长方向不一致",
                    name, cls
                ));
            }
            // 被 scaleX 横向压扁的元素**不能自己带圆角**:
            // scaleX 是非等比缩放, 圆角会跟着横向拉伸, 进度 10% 时
            // 胶囊左端会变成椭圆。圆角必须留在 track 上, 由它的
            // overflow:hidden 裁出胶囊形状。
            //
            // ⚠️ 这里必须**先去掉注释**再判 border-radius: 本文件里
            // 那条规则的解释性注释正好提到了 "border-radius", 不过滤
            // 的话守卫会对着自己的说明文字报警。
            let decls: String = rule
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.starts_with("/*") && !t.starts_with('*') && !t.contains("*/")
                })
                .collect::<Vec<_>>()
                .join("\n");
            if decls.contains("border-radius") {
                failures.push(format!(
                    "{} .{} 在被 scaleX 缩放的元素上设置了 border-radius —— \
                     圆角会随横向缩放变形(进度低时看起来像椭圆)。\
                     圆角应留在对应的 *-track 上, 靠 overflow:hidden 裁剪",
                    name, cls
                ));
            }
        }
    }

    assert!(checked > 0, "一条进度条规则都没扫到 —— 扫描器可能已失效");
    assert!(
        failures.is_empty(),
        "以下进度条仍在动画布局属性 (传输大文件时会持续掉帧):\n  {}",
        failures.join("\n  ")
    );
}

/// 假链接 `<a href="javascript:void(0)">` 不允许再出现。
///
/// 它带着链接的全部视觉暗示 (可点、有 hover、屏幕阅读器念"链接"),
/// 却没有真实目的地 —— 按下去什么也不会发生, 中键也开不出新页。
/// 用户唯一能做的推断是"这个按钮坏了"。
#[test]
fn no_javascript_void_links() {
    let files = source_files(&ui_src());
    let mut found: Vec<String> = Vec::new();

    for f in &files {
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        let raw = std::fs::read_to_string(f).unwrap_or_default();
        let src = strip_comments(&raw);
        if src.contains("javascript:void(0)") || src.contains("javascript:void(0);") {
            let line_no = src.lines().position(|l| l.contains("javascript:void(0)")).map_or(0, |i| i + 1);
            found.push(format!("{}:{} javascript:void(0)", name, line_no));
        }
    }

    assert!(
        found.is_empty(),
        "以下位置用了假链接 —— 应该改成真 <button>:\n  {}",
        found.join("\n  ")
    );
}

#[test]
fn contrast_helper_math_is_correct() {
    // 先验证对比度算法本身, 否则上面的守卫可能只是"算得不准所以没报警"。
    // WCAG 标准样例: 黑字白底应为 21:1, 白字黑底同为 21:1。
    let black = (0u8, 0u8, 0u8);
    let white = (255u8, 255u8, 255u8);
    let r = contrast_ratio(black, white);
    assert!(
        (r - 21.0).abs() < 0.1,
            "对比度算法有误: 黑对白应为 21.0, 实际 {}",
        r
    );
    // 同色对比度应为 1
    assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);
    // 解析器
    assert_eq!(parse_hex("#ffffff"), Some((255, 255, 255)));
    assert_eq!(parse_hex("#fff"), Some((255, 255, 255)));
    assert_eq!(parse_hex("rgba(255,255,255,0.2)"), None);
}
