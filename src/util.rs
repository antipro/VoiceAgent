//! 文本清洗与唤醒词匹配。
//!
//! whisper 对静音/噪声经常输出固定的“幻听”文本（例如“字幕由 XX 提供”），
//! 这里统一过滤，避免它们在 Idle 状态触发误判。

use pinyin::ToPinyin;

use crate::config::WakeConfig;

/// 常见幻觉文本（归一化后比较）。
const HALLUCINATIONS: &[&str] = &[
    "字幕由amaraorg社区提供",
    "字幕由志愿者提供",
    "请不吝点赞订阅转发打赏支持明镜与点点栏目",
    "请不吝点赞订阅转发打赏支持",
    "明镜与点点栏目",
    "中文字幕",
    "字幕制作",
    "谢谢观看",
    "谢谢大家",
    "感谢观看",
    "字幕组",
    "thankyou",
    "thanksforwatching",
    "subscribe",
    "subscribetomychannel",
];

/// 归一化：去空白、去标点、繁转简、转小写。中英文混排也适用。
pub fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .filter(|c| !is_punctuation(*c))
        .map(to_simplified)
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 常用繁体 → 简体字表（只覆盖口语高频字，足够唤醒匹配使用）。
///
/// whisper 识别中文时经常输出繁体（"小蒙同學"），不转换会导致唤醒词永远匹配不上。
/// 这不是完整的 OpenCC：只做逐字映射，不做词组和地区用词转换。
const T2S_TABLE: &str = concat!(
    "們们個个這这時时間间會会說说話话對对麼么樣样學学國国語语聽听見见覺觉來来過过還还",
    "進进開开關关買买賣卖錢钱電电腦脑機机車车飛飞場场價价現现給给讓让請请謝謝幫帮為为",
    "無无與与從从將将後后裡里邊边點点號号週周歲岁頭头臉脸體体醫医藥药愛爱媽妈兒儿爺爷",
    "誰谁嗎吗沒没別别剛刚總总數数據据網网頁页軟软盤盘應应設设選选擇择確确認认輸输聲声",
    "響响暫暂繼继續续結结氣气預预報报聞闻匯汇遊游飯饭賓宾館馆闆板師师員员碼码郵邮視视",
    "頻频圖图樂乐戲戏書书筆笔記记調调燈灯門门戶户鑰钥發发廚厨廁厕樓楼陽阳庫库鄰邻區区",
    "線线離离齡龄節节馬马該该須须歡欢討讨厭厌擔担興兴難难餓饿燒烧針针檢检驗验異异嚴严",
    "輕轻轉转復复運运動动減减壓压脈脉錄录統统計计顯显範范圍围權权註注冊册帳帐證证錶表",
    "麥麦螢荧鍵键掃扫鈕钮遙遥攝摄鈴铃講讲揚扬齒齿腳脚蓋盖膚肤臟脏腎肾腸肠經经細细兩两",
    "萬万億亿塊块幾几張张雙双隻只於于並并卻却終终雜杂專专業业長长陰阴風风雲云龍龙鳥鸟",
    "魚鱼貓猫站站傳传"
);

fn to_simplified(c: char) -> char {
    use std::collections::HashMap;
    use std::sync::OnceLock;

    static MAP: OnceLock<HashMap<char, char>> = OnceLock::new();
    MAP.get_or_init(|| {
        let chars: Vec<char> = T2S_TABLE.chars().collect();
        chars
            .chunks(2)
            .filter(|pair| pair.len() == 2)
            .map(|pair| (pair[0], pair[1]))
            .collect()
    })
    .get(&c)
    .copied()
    .unwrap_or(c)
}

fn is_punctuation(c: char) -> bool {
    matches!(
        c,
        '.' | ','
            | '!'
            | '?'
            | ';'
            | ':'
            | '\''
            | '"'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '<'
            | '>'
            | '/'
            | '\\'
            | '|'
            | '-'
            | '_'
            | '+'
            | '='
            | '*'
            | '&'
            | '^'
            | '%'
            | '$'
            | '#'
            | '@'
            | '~'
            | '`'
            | '。'
            | '，'
            | '、'
            | '！'
            | '？'
            | '；'
            | '：'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '（'
            | '）'
            | '【'
            | '】'
            | '《'
            | '》'
            | '—'
            | '…'
            | '·'
            | '～'
            | '「'
            | '」'
    )
}

/// 清理 ASR 原始输出。
pub fn clean_transcript(raw: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in raw.chars() {
        match c {
            '(' | '（' | '[' | '【' => depth += 1,
            ')' | '）' | ']' | '】' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }

    let out = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    // 去掉常见前缀标记，如 "简体中文："。
    let out = out
        .trim_start_matches(|c: char| c.is_whitespace())
        .to_string();

    let norm = normalize(&out);
    if norm.is_empty() {
        return String::new();
    }
    if HALLUCINATIONS.iter().any(|h| norm.contains(h)) {
        return String::new();
    }
    // 纯符号 / 无意义重复。
    if norm.chars().all(|c| c == norm.chars().next().unwrap()) && norm.chars().count() > 3 {
        return String::new();
    }

    out.trim().to_string()
}

/// 判断一句话是否包含唤醒词。
pub fn is_wake(cfg: &WakeConfig, text: &str) -> bool {
    strip_wake(cfg, text).is_some()
}

/// 检测唤醒词并返回「去掉唤醒词后的剩余内容」。
///
/// - 返回 `Some("")`：整句就是唤醒词。
/// - 返回 `Some("今天天气怎么样")`：用户一口气说了「小蒙同学，今天天气怎么样」，
///   调用方可以直接把剩余内容当作本轮提问，不必让用户再说一遍。
/// - 返回 `None`：没有唤醒。
pub fn strip_wake(cfg: &WakeConfig, text: &str) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let norm = normalize(text);
    if norm.is_empty() {
        return None;
    }

    // ① 精确命中：能定位就能把剩下的内容摘出来。
    for word in &cfg.words {
        let w = normalize(word);
        if w.is_empty() {
            continue;
        }
        if let Some(pos) = norm.find(&w) {
            let rest = format!("{}{}", &norm[..pos], &norm[pos + w.len()..]);
            return Some(rest);
        }
    }

    // ② 同音字命中：小蒙 / 小萌 / 小梦 / 筱蒙 / 晓蒙 的拼音完全相同。
    //    whisper 每次可能写出不同的字，按拼音比对才稳定。
    for word in &cfg.words {
        let w = normalize(word);
        if w.is_empty() {
            continue;
        }
        if let Some(rest) = strip_by_pinyin(&norm, &w) {
            return Some(rest);
        }
    }

    // ③ 模糊匹配：只知道命中了，无法安全定位，整句当作唤醒。
    let chars: Vec<char> = norm.chars().collect();
    if chars.len() <= cfg.fuzzy_max_len {
        for word in &cfg.words {
            let w: Vec<char> = normalize(word).chars().collect();
            if w.is_empty() {
                continue;
            }
            if min_window_distance(&chars, &w) <= cfg.fuzzy_max_distance {
                return Some(String::new());
            }
        }
    }
    None
}

/// 把文本转成无声调拼音，并记录每个字符在拼音串中的字节区间。
fn pinyin_with_spans(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut out = String::with_capacity(text.len() * 3);
    let mut spans = Vec::with_capacity(text.chars().count());
    for c in text.chars() {
        let start = out.len();
        if let Some(p) = c.to_pinyin() {
            out.push_str(p.plain());
        } else {
            // 非汉字（数字、字母）原样保留，保证位置能对上。
            for lower in c.to_lowercase() {
                out.push(lower);
            }
        }
        spans.push((start, out.len()));
    }
    (out, spans)
}

/// 按拼音查找唤醒词，返回去掉它之后的剩余文本。
///
/// 只在命中恰好落在字符边界上时才返回，避免在音节中间误匹配。
fn strip_by_pinyin(norm: &str, word: &str) -> Option<String> {
    let (haystack, spans) = pinyin_with_spans(norm);
    let (needle, _) = pinyin_with_spans(word);
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }

    let pos = haystack.find(&needle)?;
    let end = pos + needle.len();

    // 匹配起点/终点必须对应字符边界。
    let start_idx = spans.iter().position(|(s, _)| *s == pos)?;
    let end_idx = spans.iter().position(|(_, e)| *e == end)?;

    let rest: String = norm
        .chars()
        .enumerate()
        .filter(|(i, _)| *i < start_idx || *i > end_idx)
        .map(|(_, c)| c)
        .collect();
    Some(rest)
}

/// 在 `text` 中滑动长度接近 `word` 的窗口，返回最小编辑距离。
fn min_window_distance(text: &[char], word: &[char]) -> usize {
    let wlen = word.len();
    let mut best = usize::MAX;
    for len in [wlen.saturating_sub(1), wlen, wlen + 1] {
        if len == 0 || len > text.len() {
            continue;
        }
        for start in 0..=text.len() - len {
            let d = levenshtein(&text[start..start + len], word);
            best = best.min(d);
            if best == 0 {
                return 0;
            }
        }
    }
    best
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// 去掉 Markdown 装饰，避免 TTS 把符号念出来。
pub fn strip_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' | '`' | '#' | '>' | '|' => {}
            '[' => {
                // [文本](链接) → 文本
                let mut label = String::new();
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == ']' {
                        closed = true;
                        break;
                    }
                    label.push(c2);
                }
                if closed && chars.peek() == Some(&'(') {
                    for c2 in chars.by_ref() {
                        if c2 == ')' {
                            break;
                        }
                    }
                }
                out.push_str(&label);
            }
            _ => out.push(c),
        }
    }

    let cleaned = out
        .lines()
        .map(|line| line.trim_start_matches(['-', '•', ' ']).trim())
        .collect::<Vec<_>>()
        .join(" ");

    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WakeConfig {
        WakeConfig::default()
    }

    #[test]
    fn detects_exact_wake_word() {
        assert!(is_wake(&cfg(), "小蒙同学。"));
        assert!(is_wake(&cfg(), "嗯，小蒙同学，今天天气怎么样"));
    }

    #[test]
    fn detects_fuzzy_variant() {
        assert!(is_wake(&cfg(), "小梦同学"));
        assert!(is_wake(&cfg(), "小明同雪"));
    }

    #[test]
    fn strips_wake_word_and_keeps_question() {
        assert_eq!(
            strip_wake(&cfg(), "小蒙同学，今天天气怎么样？").as_deref(),
            Some("今天天气怎么样")
        );
        assert_eq!(strip_wake(&cfg(), "小蒙同学。").as_deref(), Some(""));
        assert_eq!(strip_wake(&cfg(), "今天天气怎么样"), None);
    }

    #[test]
    fn detects_homophones_by_pinyin() {
        // whisper 对同一段发音可能写出不同的字；拼音相同就应当唤醒。
        for variant in [
            "小蒙同学",
            "小萌同学",
            "小梦同学",
            "筱蒙同学",
            "晓蒙同学",
            "肖蒙同学",
            "小檬同学",
        ] {
            assert!(is_wake(&cfg(), variant), "should wake on {variant}");
        }
    }

    #[test]
    fn strips_homophone_wake_word_and_keeps_question() {
        assert_eq!(
            strip_wake(&cfg(), "筱蒙同学，今天天气怎么样？").as_deref(),
            Some("今天天气怎么样")
        );
    }

    #[test]
    fn pinyin_matching_does_not_fire_on_similar_sentences() {
        // 发音不同，不能误唤醒。
        for sentence in [
            "小明的同学来了",
            "明天同学聚会",
            "笑蒙了",
            "今天天气怎么样",
        ] {
            let expected = is_wake(&cfg(), sentence);
            // 只有包含完整唤醒词发音的句子才允许为真；这里全部应为否。
            assert!(!expected, "should not wake on {sentence}");
        }
    }

    #[test]
    fn detects_traditional_output_from_whisper() {
        // whisper.cpp 对普通话经常输出繁体，实测样例。
        assert!(is_wake(&cfg(), "小蒙同學,今天天氣怎麼樣?"));
        assert!(is_wake(&cfg(), "小蒙同學。"));
    }

    #[test]
    fn t2s_table_is_well_formed() {
        assert_eq!(T2S_TABLE.chars().count() % 2, 0);
    }

    #[test]
    fn ignores_normal_speech() {
        assert!(!is_wake(&cfg(), "今天天气怎么样"));
        assert!(!is_wake(&cfg(), "帮我查一下明天的航班"));
    }

    #[test]
    fn cleans_hallucinations() {
        assert_eq!(clean_transcript("字幕由 Amara.org 社区提供"), "");
        assert_eq!(clean_transcript("（音乐）"), "");
        assert_eq!(clean_transcript("你好呀"), "你好呀");
    }

    #[test]
    fn strips_markdown_for_tts() {
        assert_eq!(
            strip_markdown("**你好**，请[点击](https://a.b)查看"),
            "你好，请点击查看"
        );
        assert_eq!(strip_markdown("- 第一点\n- 第二点"), "第一点 第二点");
    }
}
