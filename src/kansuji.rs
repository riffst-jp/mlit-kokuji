//! 漢数字・全角数字を半角アラビア数字へ正規化する。
//!
//! - `千二十四` → `1024`（位取り記数法）
//! - `一〇二四` / `一○二四` → `1024`（一字ずつの表記）
//! - `１０２４` → `1024`（全角数字）
//!
//! 全角英数字・記号も半角に揃える。

fn digit_value(c: char) -> Option<u64> {
    Some(match c {
        '〇' | '○' | '零' => 0,
        '一' | '壱' => 1,
        '二' | '弐' => 2,
        '三' | '参' => 3,
        '四' => 4,
        '五' => 5,
        '六' => 6,
        '七' => 7,
        '八' => 8,
        '九' => 9,
        _ => return None,
    })
}

fn small_unit(c: char) -> Option<u64> {
    Some(match c {
        '十' | '拾' => 10,
        '百' => 100,
        '千' => 1000,
        _ => return None,
    })
}

fn large_unit(c: char) -> Option<u64> {
    Some(match c {
        '万' => 10_000,
        '億' => 100_000_000,
        _ => return None,
    })
}

fn is_kansuji(c: char) -> bool {
    digit_value(c).is_some() || small_unit(c).is_some() || large_unit(c).is_some()
}

/// 漢数字の連続を数値に変換する。解釈できなければ `None`。
fn parse_kansuji(s: &[char]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let has_unit = s.iter().any(|&c| small_unit(c).is_some() || large_unit(c).is_some());
    if !has_unit {
        // 一字ずつの表記: 一〇二四
        let mut v: u64 = 0;
        for &c in s {
            v = v.checked_mul(10)?.checked_add(digit_value(c)?)?;
        }
        return Some(v);
    }
    // 位取り表記: 千二十四, 二万三千, 一千〇二十四 など
    let mut total: u64 = 0; // 万・億で確定した分
    let mut section: u64 = 0; // 万未満の部分
    let mut digits: Option<u64> = None; // 直前の数字列（「二〇」のような混在にも対応）
    for &c in s {
        if let Some(d) = digit_value(c) {
            digits = Some(digits.unwrap_or(0) * 10 + d);
        } else if let Some(u) = small_unit(c) {
            section += digits.take().unwrap_or(1) * u;
        } else if let Some(u) = large_unit(c) {
            section += digits.take().unwrap_or(0);
            if section == 0 {
                section = 1;
            }
            total += section * u;
            section = 0;
        }
    }
    section += digits.unwrap_or(0);
    Some(total + section)
}

/// 全角英数字・記号を半角に変換する（1文字）。
fn to_halfwidth(c: char) -> char {
    match c {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{3000}' => ' ',
        _ => c,
    }
}

/// 文字列中の数字表記をすべて半角アラビア数字に正規化する。
pub fn normalize(s: &str) -> String {
    let chars: Vec<char> = s.chars().map(to_halfwidth).collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if is_kansuji(chars[i]) {
            let start = i;
            while i < chars.len() && is_kansuji(chars[i]) {
                i += 1;
            }
            let run = &chars[start..i];
            // 「万」「億」「十」単独などが一般語として現れる場合は数として扱うが、
            // 数字を一つも含まない大単位だけの並び（例: 「万一」は数字を含むので対象）
            // は誤変換を避けるためそのまま残す。
            let only_large = run.iter().all(|&c| large_unit(c).is_some());
            match (only_large, parse_kansuji(run)) {
                (false, Some(v)) => out.push_str(&v.to_string()),
                _ => out.extend(run),
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// 文字列中に現れる数字列（半角化後）を列挙する。
pub fn numbers(s: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            v.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        v.push(cur);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_of_1024() {
        for s in ["千二十四", "１０２４", "1024", "一〇二四", "一○二四", "一千二十四", "千〇二十四"] {
            assert_eq!(normalize(s), "1024", "{s}");
        }
    }

    #[test]
    fn in_context() {
        assert_eq!(normalize("国土交通省告示第千六百三十四号"), "国土交通省告示第1634号");
        assert_eq!(normalize("建設省河治発第四三号"), "建設省河治発第43号");
        assert_eq!(normalize("港管第二七七八号"), "港管第2778号");
        assert_eq!(normalize("国道利第62号"), "国道利第62号");
        assert_eq!(normalize("第三十六条の二第五号"), "第36条の2第5号");
        assert_eq!(normalize("国空安政第１５５８号"), "国空安政第1558号");
    }

    #[test]
    fn units() {
        assert_eq!(normalize("十"), "10");
        assert_eq!(normalize("二十"), "20");
        assert_eq!(normalize("百十一"), "111");
        assert_eq!(normalize("二千"), "2000");
        assert_eq!(normalize("一万二千三百"), "12300");
        assert_eq!(normalize("十万"), "100000");
    }

    #[test]
    fn numbers_extract() {
        assert_eq!(numbers("53国土政第59号"), vec!["53", "59"]);
    }
}
