//! 文本的两件基础设施：**不会被意外打印的字符串**，和**抗简单变形的归一化**。
//!
//! # [`Sensitive`]：正文不落盘的类型层保证
//!
//! 孩子的输入和模型的回答都是正文。设计稿 §7：「正文只在内存里处理」。最容易让正文漏进日志、panic 信息、
//! 错误文本的办法是 `{:?}`：一个 `#[derive(Debug)]` 的结构体里只要有一个 `String` 字段，它的 Debug 就把正文打出来。
//! 所以安全模块里所有装正文的字段都是 [`Sensitive`]：它的 `Debug` 只写字节数，**没有 `Display`**，没有 `Serialize`，
//! 想拿到文本必须显式调用 [`Sensitive::expose`]（grep 一下就能审计所有出口）。
//!
//! # [`fold`]：给规则检查器的归一化
//!
//! 规则检查器匹配的是关键词和模式，孩子（或越狱的人）最先想到的绕法是变形：大小写、全角字符、零宽字符、
//! 字母之间插空格或标点、`0`/`1`/`3` 代替字母、泰语声调符号写漏或写多、数学字母（𝐛𝐨𝐦𝐛）、带圈字母、
//! 西里尔 / 希腊字母冒充拉丁字母（`bоmb` 里的 `о` 是西里尔字母）、叠加在字母上的组合符号、重复字母（`boooomb`）。
//! 归一化把这些全部折叠掉，得到四个视图：
//!
//! | 视图 | 内容 | 用在 |
//! |---|---|---|
//! | `plain` | 小写；全角、数学字母、带圈字母、小型大写、西里尔和希腊的同形字转成对应的 ASCII 字母；泰文和阿拉伯数字转 ASCII；去掉零宽字符、组合符号、标签字符和泰语声调符号；连续空白折成一个空格 | 泰语和英语的子串、整词匹配 |
//! | `squeezed` | `plain` 去掉所有空白和标点，只留字母、数字和泰文字符 | 字母间被插了空格或标点的写法（`s u i c i d e`、`ฆ่า ตัว ตาย`） |
//! | `leet` | 先把 `@ $ 0 1 3 4 5 7` 换成对应字母，再压缩 | 用数字符号代替字母的英文写法（`k1ll myself`） |
//! | `collapsed` | `plain` 里连续重复的同一个 ASCII 字母折成一个 | 拉长字母的写法（`boooomb`、`suiciiiide`、`kiiiill`） |
//!
//! 泰语声调符号（`่ ้ ๊ ๋`）和 `์`（การันต์）在匹配前统一去掉：**文本和模式用同一个函数折叠**，所以
//! 声调写漏、写错的变体也命中，代价是极少数只靠声调区分的词会被当成同一个词（对一个失败即关闭的过滤器，这个方向是安全的）。
//! `collapsed` 视图同理：模式也折叠，所以 `kill` 和 `kiiiill` 都变成 `kil`。
//!
//! 近音拼写（`suiside`）和别的语言没有处理：那是 LLM 检查器的事，见 `llm` 模块。
//!
//! # 否定词只在命中旁边有效
//!
//! 有些规则在「警示 / 拒绝」语境里放行（`ห้ามผสม……เพราะจะเกิดก๊าซพิษ`）。**整段里随便哪里出现一个警示词就放行整段**是个漏洞
//! （r3 High#3：在有害回答末尾追加一个 `unsafe`）。[`locally_negated`] 只在命中所在的**同一句**里、命中前后各
//! [`NEGATION_REACH_CHARS`] 个字符之内找否定词，句子边界（`. ! ? ;`、换行）挡住它；一个命中被否定之后，
//! 调用方用 [`Folded::mask`] 把它抹掉，**剩下的内容重新独立检查**。
//!
//! # base64
//!
//! 有害的话可以整段编码成 base64（输入里「解码并照做」，输出里「用 base64 回答」）。[`base64_texts`] 把文本里疑似 base64 的
//! 长串解码成文本，规则检查器对解码出来的文本再查一遍（最多三层）。

use std::fmt;

/// 装正文的字符串。`Debug` 只写字节数；没有 `Display`、没有 `Serialize`。
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Sensitive(String);

impl Sensitive {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// 取出正文。每一处调用都是正文离开这个类型的地方，审计时 grep 它。
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// 字节数（可以安全地打日志）。
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Sensitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted {} bytes>", self.0.len())
    }
}

impl From<String> for Sensitive {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// 归一化之后的四个视图（见模块文档）。
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Folded {
    pub plain: String,
    pub squeezed: String,
    pub leet: String,
    /// `plain` 里连续重复的同一个 ASCII 字母折成一个（`boooomb` -> `bomb`）。
    pub collapsed: String,
    /// `plain` 里「原文这里有换行」的空格所在的字节位置：句子边界，给 [`locally_negated`] 用。
    pub breaks: Vec<usize>,
}

impl fmt::Debug for Folded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Folded(<redacted {} bytes>)", self.plain.len())
    }
}

/// 泰文字符区（U+0E00..=U+0E7F）。
fn is_thai(c: char) -> bool {
    ('\u{0E00}'..='\u{0E7F}').contains(&c)
}

/// 要丢掉的字符：零宽、格式控制、变体选择符、不可见的填充符、叠加在字母上的组合符号（删除线、下划线、变音……）、
/// 标签字符（不可见，可以夹带 ASCII）、泰语声调符号和 การันต์。
fn is_dropped(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{034F}'
        | '\u{061C}'
        | '\u{115F}'
        | '\u{1160}'
        | '\u{17B4}'
        | '\u{17B5}'
        | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{3164}'
        | '\u{FEFF}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FFA0}'
        | '\u{0E48}'..='\u{0E4C}'
        | '\u{0300}'..='\u{036F}'
        | '\u{1AB0}'..='\u{1AFF}'
        | '\u{1DC0}'..='\u{1DFF}'
        | '\u{20D0}'..='\u{20FF}'
        | '\u{FE20}'..='\u{FE2F}'
        | '\u{E0000}'..='\u{E007F}'
        | '\u{E0100}'..='\u{E01EF}')
}

/// 数学字母和数字（U+1D400..=U+1D7FF）：粗体、斜体、花体、哥特体、双线体、无衬线、等宽……共 13 种字体，每种 52 个字母。
fn math_alphanumeric(c: char) -> char {
    let code = c as u32;
    match code {
        0x1D400..=0x1D6A3 => {
            let offset = (code - 0x1D400) % 52;
            let letter = if offset < 26 {
                b'A' + offset as u8
            } else {
                b'a' + (offset - 26) as u8
            };
            char::from(letter)
        }
        0x1D6A4 => 'i',
        0x1D6A5 => 'j',
        0x1D7CE..=0x1D7FF => char::from_u32('0' as u32 + (code - 0x1D7CE) % 10).unwrap_or(c),
        _ => c,
    }
}

/// 字母式符号（U+2100..=U+214F）里数学字母的「空洞」：`ℍ`、`ℝ`、`ℎ`、`ℓ` 之类。
fn letterlike(c: char) -> char {
    match c {
        '\u{2102}' | '\u{212D}' => 'C',
        '\u{210A}' => 'g',
        '\u{210B}' | '\u{210C}' | '\u{210D}' => 'H',
        '\u{210E}' => 'h',
        '\u{2110}' | '\u{2111}' => 'I',
        '\u{2112}' => 'L',
        '\u{2113}' => 'l',
        '\u{2115}' => 'N',
        '\u{2119}' => 'P',
        '\u{211A}' => 'Q',
        '\u{211B}' | '\u{211C}' | '\u{211D}' => 'R',
        '\u{2124}' | '\u{2128}' => 'Z',
        '\u{212C}' => 'B',
        '\u{212F}' | '\u{2147}' => 'e',
        '\u{2130}' => 'E',
        '\u{2131}' => 'F',
        '\u{2133}' => 'M',
        '\u{2134}' => 'o',
        '\u{2139}' | '\u{2148}' => 'i',
        '\u{2145}' => 'D',
        '\u{2146}' => 'd',
        '\u{2149}' => 'j',
        other => other,
    }
}

/// 带圈、带括号、方框、区域指示符的字母。
fn enclosed_letter(c: char) -> char {
    let code = c as u32;
    let (base, first) = match code {
        0x249C..=0x24B5 => ('a', 0x249C),
        0x24B6..=0x24CF => ('A', 0x24B6),
        0x24D0..=0x24E9 => ('a', 0x24D0),
        0x1F110..=0x1F129 => ('A', 0x1F110),
        0x1F130..=0x1F149 => ('A', 0x1F130),
        0x1F150..=0x1F169 => ('A', 0x1F150),
        0x1F170..=0x1F189 => ('A', 0x1F170),
        0x1F1E6..=0x1F1FF => ('a', 0x1F1E6),
        _ => return c,
    };
    char::from_u32(base as u32 + (code - first)).unwrap_or(c)
}

/// 单个字符的归一化：全角转半角、各种数字转 ASCII、数学字母和带圈字母转 ASCII 字母。
fn map_char(c: char) -> char {
    match c {
        '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{0E50}'..='\u{0E59}' => char::from_u32('0' as u32 + (c as u32 - 0x0E50)).unwrap_or(c),
        '\u{0660}'..='\u{0669}' => char::from_u32('0' as u32 + (c as u32 - 0x0660)).unwrap_or(c),
        '\u{06F0}'..='\u{06F9}' => char::from_u32('0' as u32 + (c as u32 - 0x06F0)).unwrap_or(c),
        '\u{1D400}'..='\u{1D7FF}' => math_alphanumeric(c),
        '\u{2100}'..='\u{214F}' => letterlike(c),
        '\u{249C}'..='\u{24E9}' | '\u{1F110}'..='\u{1F189}' | '\u{1F1E6}'..='\u{1F1FF}' => {
            enclosed_letter(c)
        }
        other => other,
    }
}

/// 小写之后的同形字：西里尔和希腊字母、小型大写、上下标字母、带变音符号的拉丁字母，看起来像哪个 ASCII 字母就当哪个。
fn homoglyph(c: char) -> char {
    match c {
        // 西里尔
        '\u{0430}' => 'a',
        '\u{0441}' => 'c',
        '\u{0435}' | '\u{0451}' => 'e',
        '\u{043E}' => 'o',
        '\u{0440}' => 'p',
        '\u{0445}' => 'x',
        '\u{0443}' => 'y',
        '\u{0456}' | '\u{0457}' => 'i',
        '\u{0458}' => 'j',
        '\u{0455}' => 's',
        '\u{0501}' => 'd',
        '\u{04BB}' | '\u{043D}' => 'h',
        '\u{051B}' => 'q',
        '\u{051D}' => 'w',
        '\u{04CF}' => 'l',
        '\u{0475}' => 'v',
        '\u{043A}' => 'k',
        '\u{043C}' => 'm',
        '\u{0442}' => 't',
        '\u{043F}' => 'n',
        // 希腊
        '\u{03B1}' => 'a',
        '\u{03B5}' => 'e',
        '\u{03B7}' => 'n',
        '\u{03B9}' => 'i',
        '\u{03BA}' => 'k',
        '\u{03BD}' => 'v',
        '\u{03BF}' => 'o',
        '\u{03C1}' => 'p',
        '\u{03C4}' => 't',
        '\u{03C5}' => 'u',
        '\u{03C7}' => 'x',
        '\u{03F2}' => 'c',
        '\u{03F3}' => 'j',
        // 拉丁字母的变体
        '\u{0251}' => 'a',
        '\u{0261}' => 'g',
        '\u{0131}' => 'i',
        '\u{0237}' => 'j',
        '\u{017F}' => 's',
        // 小型大写
        '\u{1D00}' => 'a',
        '\u{0299}' => 'b',
        '\u{1D04}' => 'c',
        '\u{1D05}' => 'd',
        '\u{1D07}' => 'e',
        '\u{A730}' => 'f',
        '\u{0262}' => 'g',
        '\u{029C}' => 'h',
        '\u{026A}' => 'i',
        '\u{1D0A}' => 'j',
        '\u{1D0B}' => 'k',
        '\u{029F}' => 'l',
        '\u{1D0D}' => 'm',
        '\u{0274}' => 'n',
        '\u{1D0F}' => 'o',
        '\u{1D18}' => 'p',
        '\u{0280}' => 'r',
        '\u{A731}' => 's',
        '\u{1D1B}' => 't',
        '\u{1D1C}' => 'u',
        '\u{1D20}' => 'v',
        '\u{1D21}' => 'w',
        '\u{028F}' => 'y',
        '\u{1D22}' => 'z',
        // 上标、下标字母
        '\u{1D43}' | '\u{2090}' => 'a',
        '\u{1D47}' => 'b',
        '\u{1D9C}' => 'c',
        '\u{1D48}' => 'd',
        '\u{1D49}' | '\u{2091}' => 'e',
        '\u{1DA0}' => 'f',
        '\u{1D4D}' => 'g',
        '\u{02B0}' | '\u{2095}' => 'h',
        '\u{2071}' | '\u{1D62}' => 'i',
        '\u{02B2}' | '\u{2C7C}' => 'j',
        '\u{1D4F}' | '\u{2096}' => 'k',
        '\u{02E1}' | '\u{2097}' => 'l',
        '\u{1D50}' | '\u{2098}' => 'm',
        '\u{207F}' | '\u{2099}' => 'n',
        '\u{1D52}' | '\u{2092}' => 'o',
        '\u{1D56}' | '\u{209A}' => 'p',
        '\u{02B3}' | '\u{1D63}' => 'r',
        '\u{02E2}' | '\u{209B}' => 's',
        '\u{1D57}' | '\u{209C}' => 't',
        '\u{1D58}' | '\u{1D64}' => 'u',
        '\u{1D5B}' | '\u{1D65}' => 'v',
        '\u{02B7}' => 'w',
        '\u{02E3}' | '\u{2093}' => 'x',
        '\u{02B8}' => 'y',
        '\u{1DBB}' => 'z',
        // 拉丁-1 补充里带变音符号的字母
        '\u{00E0}'..='\u{00E5}' => 'a',
        '\u{00E7}' => 'c',
        '\u{00E8}'..='\u{00EB}' => 'e',
        '\u{00EC}'..='\u{00EF}' => 'i',
        '\u{00F1}' => 'n',
        '\u{00F2}'..='\u{00F6}' | '\u{00F8}' => 'o',
        '\u{00F9}'..='\u{00FC}' => 'u',
        '\u{00FD}' | '\u{00FF}' => 'y',
        other => other,
    }
}

fn leet_char(c: char) -> char {
    match c {
        '@' | '4' => 'a',
        '$' | '5' => 's',
        '0' => 'o',
        '1' => 'i',
        '3' => 'e',
        '7' => 't',
        other => other,
    }
}

fn keeps_in_squeezed(c: char) -> bool {
    c.is_alphanumeric() || is_thai(c)
}

/// 连续重复的同一个 ASCII 小写字母折成一个（`boooomb` -> `bomb`，`kill` -> `kil`）。
fn collapse_runs(plain: &str) -> String {
    let mut out = String::with_capacity(plain.len());
    let mut last: Option<char> = None;
    for c in plain.chars() {
        if c.is_ascii_lowercase() && last == Some(c) {
            continue;
        }
        out.push(c);
        last = Some(c);
    }
    out
}

impl Folded {
    /// 从 `plain` 推出另外三个视图。`breaks` 是 `plain` 里的换行位置。
    fn from_plain(plain: String, breaks: Vec<usize>) -> Self {
        let mut squeezed = String::new();
        let mut leet = String::new();
        for c in plain.chars() {
            if keeps_in_squeezed(c) {
                squeezed.push(c);
            }
            let swapped = leet_char(c);
            if keeps_in_squeezed(swapped) {
                leet.push(swapped);
            }
        }
        let collapsed = collapse_runs(&plain);
        Self {
            plain,
            squeezed,
            leet,
            collapsed,
            breaks,
        }
    }

    /// 把 `plain` 里这些字节区间抹掉（每个区间换成一个空格），其余内容照旧，重新推出全部视图。
    /// 用在「一个命中被局部的否定词放行之后，剩下的内容独立重新检查」。区间必须落在字符边界上（命中区间都是）。
    pub fn mask(&self, regions: &[(usize, usize)]) -> Folded {
        let mut sorted: Vec<(usize, usize)> = regions
            .iter()
            .copied()
            .filter(|(start, end)| start < end && *end <= self.plain.len())
            .collect();
        sorted.sort_unstable();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (start, end) in sorted {
            match spans.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => spans.push((start, end)),
            }
        }
        let mut plain = String::with_capacity(self.plain.len());
        let mut cursor = 0;
        for &(start, end) in &spans {
            plain.push_str(&self.plain[cursor..start]);
            plain.push(' ');
            cursor = end;
        }
        plain.push_str(&self.plain[cursor..]);
        // 句子边界跟着挪：区间之内的丢掉，之后的减去被抹掉的字节数（每个区间留下一个空格）
        let breaks = self
            .breaks
            .iter()
            .filter_map(|&old| {
                let mut shift = 0usize;
                for &(start, end) in &spans {
                    if old < start {
                        break;
                    }
                    if old < end {
                        return None;
                    }
                    shift += end - start - 1;
                }
                Some(old - shift)
            })
            .collect();
        Folded::from_plain(plain, breaks)
    }
}

/// 把一段文本折叠成四个视图。
pub fn fold(text: &str) -> Folded {
    let mut plain = String::with_capacity(text.len());
    let mut breaks = Vec::new();
    let mut pending_space = false;
    let mut pending_break = false;
    for original in text.chars() {
        if is_dropped(original) {
            continue;
        }
        let mapped = map_char(original);
        if mapped.is_whitespace() {
            pending_space = true;
            pending_break |= matches!(
                mapped,
                '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            );
            continue;
        }
        for lower in mapped.to_lowercase() {
            let lower = homoglyph(lower);
            if is_dropped(lower) {
                continue;
            }
            if pending_space && !plain.is_empty() {
                if pending_break {
                    breaks.push(plain.len());
                }
                plain.push(' ');
            }
            pending_space = false;
            pending_break = false;
            plain.push(lower);
        }
    }
    Folded::from_plain(plain, breaks)
}

/// 一个匹配模式的预先折叠形式。
///
/// 短模式只在 `plain` 视图里匹配：`squeezed` / `leet` 视图把空格和标点都去掉了，词的边界没了，
/// 短模式会在词与词之间撞出误报（`cook meth` 会命中 `cook method`）。所以压缩视图只用于足够长的模式：
/// 纯 ASCII 的至少 10 个字符，含泰文的至少 6 个字符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needle {
    plain: String,
    squeezed: String,
    use_squeezed: bool,
    use_leet: bool,
    /// `plain` 折叠连续重复的 ASCII 字母之后的形式（`kill` -> `kil`），和文本的 `collapsed` 视图对着匹配。
    collapsed: String,
    use_collapsed: bool,
}

/// 压缩视图只对不短于这个长度的纯 ASCII 模式启用。
const MIN_SQUEEZED_ASCII: usize = 10;
/// 压缩视图只对不短于这个长度的含泰文模式启用。
const MIN_SQUEEZED_THAI: usize = 6;
/// 折叠重复字母的视图只对至少有这么多个字母的纯 ASCII 模式启用（`xxx` 折叠成 `x`，不能拿一个字母去匹配）。
const MIN_COLLAPSED_LETTERS: usize = 4;

impl Needle {
    pub fn new(pattern: &str) -> Self {
        let folded = fold(pattern);
        let length = folded.squeezed.chars().count();
        let ascii_only = folded.squeezed.is_ascii();
        let use_squeezed = if ascii_only {
            length >= MIN_SQUEEZED_ASCII
        } else {
            length >= MIN_SQUEEZED_THAI
        };
        let use_leet = ascii_only
            && length >= MIN_SQUEEZED_ASCII
            && folded.squeezed.bytes().all(|b| b.is_ascii_lowercase());
        let use_collapsed = folded.plain.is_ascii()
            && folded
                .collapsed
                .chars()
                .filter(char::is_ascii_alphabetic)
                .count()
                >= MIN_COLLAPSED_LETTERS;
        Self {
            plain: folded.plain,
            squeezed: folded.squeezed,
            use_squeezed,
            use_leet,
            collapsed: folded.collapsed,
            use_collapsed,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.plain.is_empty()
    }

    /// 模式出现在文本的任一视图里。
    pub fn found_in(&self, haystack: &Folded) -> bool {
        if self.is_empty() {
            return false;
        }
        self.find_all(&haystack.plain, View::Plain).next().is_some()
            || (self.use_squeezed && haystack.squeezed.contains(&self.squeezed))
            || (self.use_leet && haystack.leet.contains(&self.squeezed))
            || (self.use_collapsed
                && self
                    .find_all(&haystack.collapsed, View::Collapsed)
                    .next()
                    .is_some())
    }

    /// 模式在 `plain` 视图里的所有出现位置（字节区间）。只有 `plain` 视图的位置能用来判断「否定词在不在命中旁边」。
    pub fn plain_spans(&self, haystack: &Folded) -> Vec<(usize, usize)> {
        if self.is_empty() {
            return Vec::new();
        }
        self.find_all(&haystack.plain, View::Plain).collect()
    }

    /// 某个视图下模式的所有出现位置（字节区间）。`plain` 视图里，模式两端是 ASCII 字母数字时要求落在词边界上
    /// （`cook meth` 不命中 `cook method`，`make a bomb` 不命中 `make a bombastic`）；泰文没有词边界，不检查。
    fn find_all<'a>(
        &'a self,
        text: &'a str,
        view: View,
    ) -> impl Iterator<Item = (usize, usize)> + 'a {
        let pattern = self.pattern(view);
        let bounded = matches!(view, View::Plain | View::Collapsed);
        let starts_alnum = pattern
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let ends_alnum = pattern
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let search = if pattern.is_empty() {
            None
        } else {
            Some(pattern)
        };
        search
            .into_iter()
            .flat_map(move |pattern| text.match_indices(pattern))
            .map(|(start, matched)| (start, start + matched.len()))
            .filter(move |&(start, end)| {
                if !bounded {
                    return true;
                }
                let before_ok = !starts_alnum
                    || !text[..start]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphanumeric());
                let after_ok = !ends_alnum
                    || !text[end..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric());
                before_ok && after_ok
            })
    }
}

#[derive(Clone, Copy)]
enum View {
    Plain,
    Squeezed,
    Collapsed,
}

impl Needle {
    /// 某个视图下用的模式；这个视图对该模式不启用时返回空串。
    fn pattern(&self, view: View) -> &str {
        match view {
            View::Plain => &self.plain,
            View::Squeezed if self.use_squeezed => &self.squeezed,
            View::Squeezed => "",
            View::Collapsed if self.use_collapsed => &self.collapsed,
            View::Collapsed => "",
        }
    }
}

/// `text[from..to]` 是否不超过 `gap` 个字符（`from <= to`，都在字符边界上）。
fn within(text: &str, from: usize, to: usize, gap: usize) -> bool {
    to >= from && to - from <= gap.saturating_mul(4) && text[from..to].chars().count() <= gap
}

/// 最长的模式占多少字节（给「向前找」的搜索窗口留余量；泰文 3 字节一个字符，词表里最长的模式不到 40 个字符）。
const MAX_PATTERN_BYTES: usize = 160;

/// `a` 类词出现之后的 `gap` 个字符内有 `b` 类词（`either_order` 时前后都算）。
///
/// 对抗性文本（一兆字节的 `บอกวิธี` 和 `ทำระเบิด` 交替重复）不能把它拖成平方时间：`b` 的所有出现位置排好序，
/// 对每个 `a` 的出现位置只二分查找窗口内的那几个，总共 `O(A log B + 窗口内命中数)`。
pub fn near(haystack: &Folded, a: &[Needle], b: &[Needle], gap: usize, either_order: bool) -> bool {
    let reach = gap.saturating_mul(4);
    for (text, view) in [
        (haystack.plain.as_str(), View::Plain),
        (haystack.squeezed.as_str(), View::Squeezed),
        (haystack.collapsed.as_str(), View::Collapsed),
    ] {
        let mut b_spans: Vec<(usize, usize)> = b
            .iter()
            .flat_map(|needle| needle.find_all(text, view))
            .collect();
        if b_spans.is_empty() {
            continue;
        }
        b_spans.sort_unstable();
        for a_needle in a {
            for (a_start, a_end) in a_needle.find_all(text, view) {
                // 向后：第一个从 a_end 开始或之后开始的 b，到窗口尽头
                let first_after = b_spans.partition_point(|&(start, _)| start < a_end);
                for &(b_start, _) in &b_spans[first_after..] {
                    if b_start - a_end > reach {
                        break;
                    }
                    if within(text, a_end, b_start, gap) {
                        return true;
                    }
                }
                // 向前：a_start 之前结束的 b。b 按起点排序，从 a_start 往回走到窗口尽头
                if either_order {
                    let last_before = b_spans.partition_point(|&(start, _)| start < a_start);
                    for &(b_start, b_end) in b_spans[..last_before].iter().rev() {
                        if a_start.saturating_sub(b_start) > reach + MAX_PATTERN_BYTES {
                            break;
                        }
                        if b_end <= a_start && within(text, b_end, a_start, gap) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// 同 [`near`] 的 `plain` 视图，但返回每一处命中的区间 `(起点, 终点)`（覆盖 `a` 和 `b` 两个词），
/// 给「命中旁边有没有否定词」用。最多收集 `limit + 1` 个（调用方看到超过 `limit` 个就当作不能逐个判断）。
pub fn near_regions(
    haystack: &Folded,
    a: &[Needle],
    b: &[Needle],
    gap: usize,
    either_order: bool,
    limit: usize,
) -> Vec<(usize, usize)> {
    let reach = gap.saturating_mul(4);
    let text = haystack.plain.as_str();
    let mut b_spans: Vec<(usize, usize)> = b
        .iter()
        .flat_map(|needle| needle.find_all(text, View::Plain))
        .collect();
    if b_spans.is_empty() {
        return Vec::new();
    }
    b_spans.sort_unstable();
    let mut regions = Vec::new();
    for a_needle in a {
        for (a_start, a_end) in a_needle.find_all(text, View::Plain) {
            let first_after = b_spans.partition_point(|&(start, _)| start < a_end);
            for &(b_start, b_end) in &b_spans[first_after..] {
                if b_start - a_end > reach {
                    break;
                }
                if within(text, a_end, b_start, gap) {
                    regions.push((a_start, b_end));
                    if regions.len() > limit {
                        return regions;
                    }
                }
            }
            if either_order {
                let last_before = b_spans.partition_point(|&(start, _)| start < a_start);
                for &(b_start, b_end) in b_spans[..last_before].iter().rev() {
                    if a_start.saturating_sub(b_start) > reach + MAX_PATTERN_BYTES {
                        break;
                    }
                    if b_end <= a_start && within(text, b_end, a_start, gap) {
                        regions.push((b_start, a_end));
                        if regions.len() > limit {
                            return regions;
                        }
                    }
                }
            }
        }
    }
    regions
}

/// 英文整词匹配：文本按「非 ASCII 字母数字」切词，词和 `words` 中的某个相等。
///
/// 另外，**写了重复字母的词**（`fuuuck`、`shiiit`）折叠之后和折叠过的 `words` 比一遍（只比不短于 4 个字母的词）。
/// 只看「自己带重复字母」的词：`Niger`（国家）折叠后和 `nigger` 折叠后一样，但它没有重复字母，不能因此被当成脏话。
pub fn has_word(haystack: &Folded, words: &[&str]) -> bool {
    let tokens = || {
        haystack
            .plain
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
    };
    if tokens().any(|token| words.contains(&token)) {
        return true;
    }
    let collapsed_words: Vec<String> = words
        .iter()
        .map(|word| collapse_runs(&fold(word).plain))
        .filter(|word| word.len() >= MIN_COLLAPSED_LETTERS)
        .collect();
    if collapsed_words.is_empty() {
        return false;
    }
    tokens().any(|token| {
        let collapsed = collapse_runs(token);
        collapsed != token && collapsed_words.contains(&collapsed)
    })
}

/// 否定词离命中最多多少个字符还算「旁边」（命中前后各一份）。
pub const NEGATION_REACH_CHARS: usize = 30;

/// 一句话的结尾：句号、问号、叹号、分号、省略号。全角的问号和叹号折叠时已经变成 ASCII。
fn ends_a_clause(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | ';' | '\u{3002}' | '\u{2026}')
}

/// 命中区间所在的那一句：从区间起点向前、终点向后各最多 `reach` 个字符，碰到句子结尾或换行就停。
fn clause_window(haystack: &Folded, (lo, hi): (usize, usize), reach: usize) -> (usize, usize) {
    let plain = &haystack.plain;
    let mut start = lo;
    for (taken, (index, c)) in plain[..lo].char_indices().rev().enumerate() {
        if taken >= reach || ends_a_clause(c) || haystack.breaks.binary_search(&index).is_ok() {
            break;
        }
        start = index;
    }
    let mut finish = hi;
    for (taken, (offset, c)) in plain[hi..].char_indices().enumerate() {
        let index = hi + offset;
        if taken >= reach || ends_a_clause(c) || haystack.breaks.binary_search(&index).is_ok() {
            break;
        }
        finish = index + c.len_utf8();
    }
    (start, finish)
}

/// 命中区间 `region` 是不是被同一句里的否定词（`cues` 里的任何一个，在命中前后各 `reach_chars` 个字符之内）放行了。
/// **只看命中所在的那一句**：别的句子里的否定词、整段末尾追加的 `unsafe` 都不算。
pub fn locally_negated(
    haystack: &Folded,
    region: (usize, usize),
    cues: &[Needle],
    reach_chars: usize,
) -> bool {
    if region.0 > region.1 || region.1 > haystack.plain.len() {
        return false;
    }
    let (start, finish) = clause_window(haystack, region, reach_chars);
    let window = Folded::from_plain(haystack.plain[start..finish].to_string(), Vec::new());
    cues.iter().any(|cue| cue.found_in(&window))
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------

/// 单个词至少多长才试着当 base64 解（再短的话不太可能装得下一个有害短语）。
const MIN_BASE64_WORD: usize = 12;
/// 被空白拆开的 base64（MIME 换行）：每一段至少多长才算候选。
const MIN_BASE64_PIECE: usize = 8;
/// 一段文本里最多接受多少个「解出来像文本」的 base64 串；再多就是有人在往检查器里灌东西（每一个都要整套规则再查一遍），
/// 调用方按失败即关闭处理。
pub const MAX_BASE64_TEXTS: usize = 32;

/// [`base64_texts`] 的结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Base64Texts {
    /// 解出来像文本的串。
    pub texts: Vec<String>,
    /// 像文本的串多于 [`MAX_BASE64_TEXTS`]：没有全部收集，调用方按失败即关闭拒绝。
    pub overflow: bool,
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// 标准和 URL 安全两种字母表都认，填充可有可无。长度除以 4 余 1 不可能是 base64。
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let trimmed = text.trim_end_matches('=');
    if trimmed.is_empty() || trimmed.len() % 4 == 1 || text.len() - trimmed.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len() / 4 * 3 + 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in trimmed.bytes() {
        buffer = (buffer << 6) | u32::from(base64_value(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

/// 解出来的字节像文本吗：合法的 UTF-8，至少 4 个字符，其中 90% 以上是字母数字、空白、ASCII 标点或泰文，没有控制字符。
fn as_text(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    let mut total = 0usize;
    let mut texty = 0usize;
    for c in text.chars() {
        total += 1;
        if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
            return None;
        }
        if c.is_alphanumeric() || c.is_whitespace() || c.is_ascii_punctuation() || is_thai(c) {
            texty += 1;
        }
    }
    (total >= 4 && texty * 10 >= total * 9).then_some(text)
}

/// 文本里疑似 base64 的长串解码成的文本。
///
/// 候选有两种：单个足够长的词（只含 base64 字母表），和被空白拆开的连续几段（MIME 的换行）拼起来。
/// 解出来不像文本的（二进制、乱码、哈希）丢掉：一个英文长单词、一个十六进制摘要也满足字母表，但解出来几乎总是乱码。
/// 每个词至多被当单个候选解一次、至多在一个分组里再解一次，所以总工作量是 `O(文本长度)`，输入决定不了检查的代价；
/// 解码出来的文本里可以再有 base64，由调用方对结果再查一遍并限制层数。
pub fn base64_texts(text: &str) -> Base64Texts {
    fn accept(decoded: Option<String>, out: &mut Base64Texts) {
        if let Some(decoded) = decoded {
            if out.texts.len() >= MAX_BASE64_TEXTS {
                out.overflow = true;
            } else {
                out.texts.push(decoded);
            }
        }
    }
    fn flush(group: &mut Vec<&str>, out: &mut Base64Texts) {
        if group.len() >= 2 {
            accept(decode_base64(&group.concat()).and_then(as_text), out);
        }
        group.clear();
    }
    let is_piece = |word: &str| {
        let body = word.trim_end_matches('=');
        !body.is_empty()
            && word.len() - body.len() <= 2
            && body.bytes().all(|b| base64_value(b).is_some())
    };
    let mut out = Base64Texts::default();
    let mut group: Vec<&str> = Vec::new();
    for word in text.split(char::is_whitespace).filter(|w| !w.is_empty()) {
        if !is_piece(word) {
            flush(&mut group, &mut out);
            continue;
        }
        if word.trim_end_matches('=').len() >= MIN_BASE64_WORD {
            accept(decode_base64(word).and_then(as_text), &mut out);
        }
        if word.len() >= MIN_BASE64_PIECE {
            group.push(word);
        } else {
            flush(&mut group, &mut out);
        }
    }
    flush(&mut group, &mut out);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn sensitive_never_shows_its_text() {
        let secret = Sensitive::new("SECRET-SENTINEL-1234");
        let shown = format!("{secret:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("20 bytes"), "{shown}");
        // 嵌在派生 Debug 的结构里同样安全
        #[derive(Debug)]
        struct Holder {
            _text: Sensitive,
            _many: Vec<Sensitive>,
            _maybe: Option<Sensitive>,
        }
        let holder = Holder {
            _text: secret.clone(),
            _many: vec![secret.clone()],
            _maybe: Some(secret.clone()),
        };
        assert!(!format!("{holder:?}").contains("SECRET"));
        assert!(!format!("{holder:#?}").contains("SECRET"));
        assert_eq!(secret.expose(), "SECRET-SENTINEL-1234");
        assert!(!format!("{:?}", fold("SECRET-SENTINEL")).contains("SECRET"));
    }

    #[test]
    fn folding_defeats_the_cheap_evasions() {
        let folded = fold("  K i L l   MY\u{200B}SELF  ");
        // 大小写折叠、零宽字符丢掉、连续空白折成一个空格、首尾空白去掉
        assert_eq!(folded.plain, "k i l l myself");
        assert_eq!(folded.squeezed, "killmyself");
        // 全角
        assert_eq!(fold("ＫＩＬＬ").plain, "kill");
        // 数字和符号代替字母
        assert_eq!(fold("k1ll my$elf").leet, "killmyself");
        assert_eq!(fold("h@ck").leet, "hack");
        // 泰文数字和阿拉伯-印度数字
        assert_eq!(fold("๐๘๐-๐๐๐-๑๐๐๑").plain, "080-000-1001");
        assert_eq!(fold("٠٨٠").plain, "080");
    }

    #[test]
    fn thai_tone_marks_are_folded_on_both_sides() {
        // 声调写漏、写多的变体和标准写法折叠成同一个串
        assert_eq!(fold("ฆ่าตัวตาย").plain, fold("ฆาตัวตาย").plain);
        assert_eq!(fold("ฆ่าตัวตาย").plain, fold("ฆ้าตัวตาย").plain);
        let needle = Needle::new("ฆ่าตัวตาย");
        assert!(needle.found_in(&fold("หนูอยากฆาตัวตาย")));
        // 字之间被插了空格
        assert!(needle.found_in(&fold("ฆ่า ตัว ตาย")));
        // 毫不相干的文本
        assert!(!needle.found_in(&fold("วันนี้อากาศดี")));
    }

    #[test]
    fn needles_match_the_plain_squeezed_and_leet_views() {
        let needle = Needle::new("kill myself");
        assert!(needle.found_in(&fold("I will kill myself")));
        assert!(needle.found_in(&fold("k.i.l.l m-y-s-e-l-f")));
        assert!(needle.found_in(&fold("k1ll my$elf")));
        assert!(!needle.found_in(&fold("kill the process then reload myself page")));
        // 太短的模式不走 leet 视图（避免数字文本误伤）
        // 太短的模式只在 plain 视图里匹配：压缩视图没有词边界，短模式会在词与词之间撞出误报
        let short = Needle::new("die");
        assert!(!short.use_leet && !short.use_squeezed);
        assert!(!short.found_in(&fold("d13")));
        assert!(Needle::new("kill myself").use_leet);
        // plain 视图里 ASCII 模式要落在词边界上：`cook meth` 不命中 `cook method`
        let cook = Needle::new("cook meth");
        assert!(cook.found_in(&fold("they cook meth at night")));
        assert!(!cook.found_in(&fold("the cook method for rice")));
        let bomb = Needle::new("make a bomb");
        assert!(bomb.found_in(&fold("How to make a bomb?")));
        assert!(!bomb.found_in(&fold("make a bombastic speech")));
        assert!(!bomb.found_in(&fold("remake a bomb")));
    }

    #[test]
    fn near_respects_the_gap_the_order_and_both_views() {
        let a = [Needle::new("บอก")];
        let b = [Needle::new("ระเบิด")];
        assert!(near(&fold("บอกวิธีทำระเบิด"), &a, &b, 14, false));
        // 超出间隔
        assert!(!near(
            &fold("บอกอะไรก็ได้ที่ไม่เกี่ยวข้องกับเรื่องนี้เลยนะแล้วค่อยพูดถึงระเบิด"),
            &a,
            &b,
            5,
            false
        ));
        // 顺序相反：只有 either_order 才算
        assert!(!near(&fold("ระเบิดบอก"), &a, &b, 10, false));
        assert!(near(&fold("ระเบิดบอก"), &a, &b, 10, true));
        // 被插了空格也命中（squeezed 视图；只对足够长的模式启用）
        let long_a = [Needle::new("สอนวิธี")];
        let long_b = [Needle::new("ทำระเบิด")];
        assert!(near(
            &fold("ส อ น วิ ธี ทำ ร ะ เ บิ ด"),
            &long_a,
            &long_b,
            5,
            false
        ));
        // 短模式不走压缩视图：词与词之间撞出的误报比漏掉一种变形更糟
        assert!(!near(
            &fold("บ อ ก ร ะ เ บิ ด"),
            &a,
            &[Needle::new("ระเบิด")],
            12,
            false
        ));
    }

    #[test]
    fn near_is_not_quadratic_on_adversarial_text() {
        // 一大片密集的 A、隔一段空白、再一大片密集的 B：两边各上万个出现位置，但没有任何一对在窗口内。
        // O(A x B) 的实现要做上亿次比较；二分之后只做 O(A log B)。
        let a = [Needle::new("บอกวิธี")];
        let b = [Needle::new("ทำระเบิด")];
        let text = format!(
            "{}{}{}",
            "บอกวิธี".repeat(15_000),
            "ก".repeat(1_000),
            "ทำระเบิด".repeat(15_000)
        );
        let folded = fold(&text);
        assert!(folded.plain.len() > 600_000);
        let started = std::time::Instant::now();
        assert!(!near(&folded, &a, &b, 10, true));
        assert!(!near(&folded, &a, &b, 10, false));
        // 间隔放宽到 1500 个字符：最后一个 A 和第一个 B 相隔 1000 个字符，命中
        assert!(near(&folded, &a, &b, 1_500, false));
        assert!(near(&folded, &a, &b, 1_500, true));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn whole_word_matching_does_not_hit_inside_longer_words() {
        let folded = fold("Bombay is a city; the shitake mushroom; assassin");
        assert!(!has_word(&folded, &["bomb", "shit", "ass"]));
        assert!(has_word(&fold("what a SHIT day"), &["shit"]));
        assert!(has_word(&fold("f*ck, shit!"), &["shit"]));
    }
}
