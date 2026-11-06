use anyhow::{Context, Result, anyhow, bail};
use calamine::{Data, Reader, Xlsx, open_workbook_from_rs};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Column;
use crate::kansuji;

pub const SITE: &str = "https://www.mlit.go.jp";
pub const INDEX_URL: &str = "https://www.mlit.go.jp/notice/index.html";

pub fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("mlkokuji/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("HTTP client")
}

pub fn fetch(url: &str) -> Result<Vec<u8>> {
    let res = client()
        .get(url)
        .send()
        .with_context(|| format!("取得に失敗しました: {url}"))?
        .error_for_status()
        .with_context(|| format!("取得に失敗しました: {url}"))?;
    Ok(res.bytes()?.to_vec())
}

// ---------------------------------------------------------------------------
// 一覧データ

/// 告示・通達 1 件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub title: String,
    pub number: String,
    /// Excel シリアル日（1899-12-30 起点）
    pub date: Option<i64>,
    /// 日付が文字列で入っている場合の原文
    pub date_raw: String,
    pub org: String,
    pub url: String,
}

/// 検索用の派生情報を持つ 1 件
#[derive(Debug, Clone)]
pub struct Entry {
    pub rec: Record,
    /// 半角英数字に正規化した文書番号（検索用）
    pub number: String,
    /// 表示用の文書番号（設定 `normalize_number` に従う。`set_number_format` で切り替え）
    pub shown_number: String,
    /// 表示用日付 (YYYY-MM-DD)
    pub date: String,
    /// 和暦 (例: H12.05.31)
    pub era: String,
    /// 所轄（局レベルに短縮した組織名）
    pub org_short: String,
    /// 文書番号中の数字列
    pub nums: Vec<String>,
    /// 検索対象文字列（正規化・小文字化済み）
    hay: String,
}

impl Entry {
    pub fn new(mut rec: Record) -> Self {
        // セル内改行（複数の文書番号の併記など）を1行にまとめる
        let one_line = |s: &str, sep: &str| s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(sep);
        rec.title = one_line(&rec.title, " ");
        rec.org = one_line(&rec.org, " ");
        rec.number = one_line(&rec.number, " / ");
        let number = kansuji::normalize(&one_line(&rec.number, " / "));
        // 「第１ ０ 号」のような数字間の空白を除去
        let nc: Vec<char> = number.chars().collect();
        let number: String = (0..nc.len())
            .filter(|&i| {
                !(nc[i] == ' '
                    && i > 0
                    && nc[i - 1].is_ascii_digit()
                    && nc.get(i + 1).is_some_and(|c| c.is_ascii_digit()))
            })
            .map(|i| nc[i])
            .collect();
        let (date, era, era_long) = match rec.date {
            Some(d) => {
                let (y, m, dd) = civil_from_serial(d);
                let (e, el) = era(y, m, dd);
                (format!("{y:04}-{m:02}-{dd:02}"), e, el)
            }
            None => (kansuji::normalize(&rec.date_raw), String::new(), String::new()),
        };
        let nums = kansuji::numbers(&number);
        let hay = format!(
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            number,
            kansuji::normalize(&rec.title),
            date,
            era,
            era_long,
            kansuji::normalize(&rec.org)
        )
        .to_lowercase();
        let org_short = short_org(&rec.org, &number);
        Entry { rec, shown_number: number.clone(), number, date, era, org_short, nums, hay }
    }

    /// 表の列の値
    pub fn column(&self, c: Column) -> &str {
        match c {
            Column::Org => &self.org_short,
            Column::OrgFull => &self.rec.org,
            Column::Number => &self.shown_number,
            Column::Date => &self.date,
            Column::Era => &self.era,
            Column::Title => &self.rec.title,
        }
    }

    pub fn ext(&self) -> &str {
        url_ext(&self.rec.url)
    }

    /// 保存用ファイル名
    pub fn file_name(&self) -> String {
        let title: String = self.rec.title.chars().take(60).collect();
        let base = format!("{}_{}", self.shown_number, title);
        let base: String = base
            .chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                c if c.is_control() => '_',
                c => c,
            })
            .collect();
        let ext = self.ext();
        if ext.is_empty() { format!("{base}.html") } else { format!("{base}.{ext}") }
    }
}

/// 組織名を一覧表示用に短縮する。
/// 「国土交通省住宅局建築指導課」→「住宅局」、「建設省河川局長」→「河川局」、
/// 局が無ければ省庁名、組織名が空なら文書番号の省庁名。
pub fn short_org(org: &str, number: &str) -> String {
    let ministry = |s: &str| -> Option<String> {
        let i = s.char_indices().take(8).find(|&(_, c)| c == '省' || c == '庁')?.0;
        Some(s[..i + '省'.len_utf8()].to_string())
    };
    let org = org.trim().trim_start_matches(['／', '/']);
    let org = org.strip_suffix("通知").unwrap_or(org).trim();
    if org.is_empty() {
        return ministry(number).or_else(|| bureau_from_number(number)).unwrap_or_default();
    }
    let rest = match ministry(org) {
        Some(m) if org.len() > m.len() => &org[m.len()..],
        _ => org,
    };
    if let Some((i, _)) = rest.char_indices().take(9).find(|&(_, c)| c == '局') {
        return rest[..i + '局'.len_utf8()].to_string();
    }
    match ministry(org) {
        Some(m) => m,
        None => org.chars().take(8).collect(),
    }
}

/// 文書番号の略号（国住指、住指発 など）から局を推定する
fn bureau_from_number(number: &str) -> Option<String> {
    const TABLE: &[(&str, &str)] = &[
        ("国住", "住宅局"), ("住", "住宅局"), ("発住", "住宅局"),
        ("国道", "道路局"),
        ("国空", "航空局"), ("空", "航空局"),
        ("国都", "都市局"), ("都", "都市局"), ("建都", "都市局"),
        ("国自", "自動車局"),
        ("国海", "海事局"), ("国港", "港湾局"), ("国鉄", "鉄道局"),
        ("国河", "河川局"), ("河", "河川局"), ("国水", "水管理・国土保全局"),
        ("国総", "総合政策局"), ("国官", "大臣官房"),
        ("国土", "国土庁"), ("経", "建設経済局"), ("運", "運輸省"),
    ];
    let head = number.trim_start_matches(|c: char| c.is_ascii_digit() || c.is_whitespace());
    let head = head.strip_prefix("建設省").unwrap_or(head);
    TABLE.iter().find(|(k, _)| head.starts_with(k)).map(|(_, v)| v.to_string())
}

/// 表示用の文書番号を、半角英数字に整形するか原文のままにするか切り替える
pub fn set_number_format(entries: &mut [Entry], normalize: bool) {
    for e in entries {
        e.shown_number = if normalize { e.number.clone() } else { e.rec.number.clone() };
    }
}

pub fn url_ext(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    match last.rsplit_once('.') {
        Some((_, e)) if e.len() <= 5 => e,
        _ => "",
    }
}

/// Excel シリアル日 → (年, 月, 日)
pub fn civil_from_serial(serial: i64) -> (i64, u32, u32) {
    // 1899-12-30 は 1970-01-01 の 25569 日前
    let z = serial - 25569 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 和暦（短縮形, 長形）
fn era(y: i64, m: u32, d: u32) -> (String, String) {
    let ymd = y * 10000 + m as i64 * 100 + d as i64;
    let (s, l, start) = if ymd >= 20190501 {
        ("R", "令和", 2018)
    } else if ymd >= 19890108 {
        ("H", "平成", 1988)
    } else if ymd >= 19261225 {
        ("S", "昭和", 1925)
    } else if ymd >= 19120730 {
        ("T", "大正", 1911)
    } else {
        ("M", "明治", 1867)
    };
    let n = y - start;
    (format!("{s}{n}.{m:02}.{d:02}"), format!("{l}{n}年{m}月{d}日"))
}

// ---------------------------------------------------------------------------
// 検索

pub struct Query {
    terms: Vec<Term>,
}

enum Term {
    /// 文書番号中の数字と完全一致
    Number(String),
    Text(String),
    Not(String),
}

impl Query {
    pub fn parse(q: &str) -> Query {
        let q = kansuji::normalize(q).to_lowercase();
        let terms = q
            .split_whitespace()
            .filter_map(|t| {
                if let Some(n) = t.strip_prefix('!') {
                    (!n.is_empty()).then(|| Term::Not(n.to_string()))
                } else if t.chars().all(|c| c.is_ascii_digit()) {
                    Some(Term::Number(t.trim_start_matches('0').to_string()))
                } else {
                    Some(Term::Text(t.to_string()))
                }
            })
            .collect();
        Query { terms }
    }

    pub fn matches(&self, e: &Entry) -> bool {
        self.terms.iter().all(|t| match t {
            Term::Number(n) => e.nums.iter().any(|x| x.trim_start_matches('0') == n),
            Term::Text(s) => e.hay.contains(s.as_str()),
            Term::Not(s) => !e.hay.contains(s.as_str()),
        })
    }

    pub fn filter(&self, entries: &[Entry]) -> Vec<usize> {
        (0..entries.len()).filter(|&i| self.matches(&entries[i])).collect()
    }
}

// ---------------------------------------------------------------------------
// キャッシュと更新

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    /// 一覧 Excel の URL（更新ごとにファイル名が変わる）
    pub xlsx_url: String,
    /// サイト上の「最終更新日」表記
    pub site_updated: String,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    meta: Meta,
    records: Vec<Record>,
}

pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Store { dir }
    }

    fn data_path(&self) -> PathBuf {
        self.dir.join("notices.json")
    }

    pub fn pages_dir(&self) -> PathBuf {
        self.dir.join("pages")
    }

    pub fn exists(&self) -> bool {
        self.data_path().exists()
    }

    pub fn load(&self) -> Result<(Meta, Vec<Entry>)> {
        let p = self.data_path();
        let s = std::fs::read(&p).with_context(|| format!("キャッシュを読めません: {}", p.display()))?;
        let c: CacheFile = serde_json::from_slice(&s).context("キャッシュが壊れています。`mlkokuji update` を実行してください")?;
        Ok((c.meta, c.records.into_iter().map(Entry::new).collect()))
    }

    fn save(&self, meta: &Meta, records: &[Record], xlsx: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.dir.join("notices.xlsx"), xlsx)?;
        let c = CacheFile { meta: meta.clone(), records: records.to_vec() };
        let tmp = self.data_path().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&c)?)?;
        std::fs::rename(tmp, self.data_path())?;
        Ok(())
    }

    /// サイトから最新の一覧を取得して保存する
    pub fn update(&self, remote: Option<Meta>) -> Result<(Meta, Vec<Entry>)> {
        let meta = match remote {
            Some(m) => m,
            None => remote_meta()?,
        };
        let xlsx = fetch(&meta.xlsx_url)?;
        let mut records = parse_xlsx(&xlsx)?;
        records.sort_by_key(|r| std::cmp::Reverse(r.date));
        self.save(&meta, &records, &xlsx)?;
        Ok((meta, records.into_iter().map(Entry::new).collect()))
    }

    pub fn clean(&self) -> Result<()> {
        if self.dir.exists() {
            std::fs::remove_dir_all(&self.dir)?;
        }
        Ok(())
    }
}

/// 告示一覧ページから Excel の URL と最終更新日を取得
pub fn remote_meta() -> Result<Meta> {
    let html = decode_html(&fetch(INDEX_URL)?);
    let re = regex::Regex::new(r#"href="([^"]+\.xlsx)""#).unwrap();
    let links: Vec<&str> = re.captures_iter(&html).map(|c| c.get(1).unwrap().as_str()).collect();
    let href = match links.as_slice() {
        [] => bail!("告示一覧ページに Excel ファイルが見つかりません: {INDEX_URL}"),
        [h, ..] => *h,
    };
    let xlsx_url = absolutize(href);
    let site_updated = regex::Regex::new(r"最終更新日[:：]\s*([0-9０-９]+年[0-9０-９]+月[0-9０-９]+日)")
        .unwrap()
        .captures(&html)
        .map(|c| kansuji::normalize(&c[1]))
        .unwrap_or_default();
    Ok(Meta { xlsx_url, site_updated })
}

fn absolutize(href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{SITE}{href}")
    } else {
        format!("{SITE}/notice/{href}")
    }
}

/// 更新の有無を確認。更新があれば新しい Meta を返す。
pub fn check_update(local: &Meta) -> Result<Option<Meta>> {
    let remote = remote_meta()?;
    Ok((remote != *local).then_some(remote))
}

fn cell_str(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Float(f) => f.to_string(),
        Data::Int(i) => i.to_string(),
        other => other.to_string().trim().to_string(),
    }
}

pub fn parse_xlsx(bytes: &[u8]) -> Result<Vec<Record>> {
    let mut wb: Xlsx<_> = open_workbook_from_rs(Cursor::new(bytes)).context("Excel ファイルを開けません")?;
    let range = wb
        .worksheet_range_at(0)
        .ok_or_else(|| anyhow!("Excel ファイルにシートがありません"))??;

    let rows: Vec<&[Data]> = range.rows().collect();
    let header_idx = rows
        .iter()
        .position(|r| r.iter().any(|c| cell_str(c) == "文書番号"))
        .ok_or_else(|| anyhow!("Excel のヘッダー行が見つかりません（形式が変わった可能性があります）"))?;
    let header: Vec<String> = rows[header_idx].iter().map(cell_str).collect();
    let col = |name: &str| header.iter().position(|h| h.contains(name));
    let c_title = col("名称").ok_or_else(|| anyhow!("「名称」列がありません"))?;
    let c_num = col("文書番号").unwrap();
    let c_date = col("年月日");
    let c_org = col("組織");
    let c_url = header
        .iter()
        .position(|h| h.eq_ignore_ascii_case("url"))
        .ok_or_else(|| anyhow!("「URL」列がありません"))?;

    let get = |r: &[Data], c: usize| r.get(c).map(cell_str).unwrap_or_default();
    let mut out = Vec::new();
    for r in &rows[header_idx + 1..] {
        let title = get(r, c_title);
        let url = get(r, c_url);
        if title.is_empty() && url.is_empty() {
            continue;
        }
        let (date, date_raw) = match c_date.and_then(|c| r.get(c)) {
            Some(Data::DateTime(dt)) => (Some(dt.as_f64() as i64), String::new()),
            Some(Data::Float(f)) => (Some(*f as i64), String::new()),
            Some(Data::Int(i)) => (Some(*i), String::new()),
            Some(d) => (None, cell_str(d)),
            None => (None, String::new()),
        };
        out.push(Record {
            title,
            number: get(r, c_num),
            date,
            date_raw,
            org: c_org.map(|c| get(r, c)).unwrap_or_default(),
            url: url.trim().to_string(),
        });
    }
    if out.is_empty() {
        bail!("Excel ファイルにデータがありません");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 本文

pub fn decode_html(bytes: &[u8]) -> String {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let re = regex::Regex::new(r#"(?i)charset\s*=\s*["']?([A-Za-z0-9_\-]+)"#).unwrap();
    let enc = re
        .captures(&head)
        .and_then(|c| encoding_rs::Encoding::for_label(c[1].as_bytes()))
        .unwrap_or(if std::str::from_utf8(bytes).is_ok() {
            encoding_rs::UTF_8
        } else {
            encoding_rs::SHIFT_JIS
        });
    enc.decode(bytes).0.into_owned()
}

/// URL に対応するローカルキャッシュのパス
pub fn page_cache_path(store: &Store, url: &str) -> PathBuf {
    let key: String = url
        .split_once("://")
        .map(|x| x.1)
        .unwrap_or(url)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    store.pages_dir().join(key)
}

/// 本文を取得（キャッシュ優先）
pub fn fetch_page(store: &Store, url: &str) -> Result<(PathBuf, Vec<u8>)> {
    let p = page_cache_path(store, url);
    if let Ok(b) = std::fs::read(&p) {
        return Ok((p, b));
    }
    let b = fetch(url)?;
    std::fs::create_dir_all(store.pages_dir())?;
    std::fs::write(&p, &b)?;
    Ok((p, b))
}

/// プレビュー用テキストを生成
pub fn preview_text(store: &Store, e: &Entry) -> Result<String> {
    let url = &e.rec.url;
    let ext = e.ext().to_ascii_lowercase();
    if !url.starts_with("http") {
        bail!("URL がありません");
    }
    let looks_html = matches!(ext.as_str(), "html" | "htm" | "");
    if !looks_html && !matches!(ext.as_str(), "pdf") {
        return Ok(format!("（{ext} ファイルはプレビューできません。s で開いてください）\n\n{url}"));
    }
    let (path, bytes) = fetch_page(store, url)?;
    if bytes.starts_with(b"%PDF") {
        return pdf_text(&path);
    }
    if !looks_html {
        bail!("PDF ではないデータが返されました");
    }
    Ok(html_text(&bytes))
}

pub fn html_text(bytes: &[u8]) -> String {
    let html = decode_html(bytes);
    // デコード済みなので charset 指定を除去（html2text に再解釈させない）
    let html = regex::Regex::new(r#"(?i)<meta[^>]*charset[^>]*>"#).unwrap().replace_all(&html, "");
    let text = html2text::config::plain_no_decorate()
        .no_link_wrapping()
        .string_from_read(html.as_bytes(), 10_000)
        .unwrap_or_else(|_| html.to_string());
    tidy(&text)
}

fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        // 表の罫線だけの行は捨てる
        if !line.trim().is_empty() && line.chars().all(|c| c.is_whitespace() || "─━═┼┬┴├┤│-".contains(c)) {
            continue;
        }
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(&line.replace('○', "〇")); // ○ は端末により幅が異なり枠線が崩れるため
        out.push('\n');
    }
    out.trim().to_string()
}

fn pdf_text(path: &Path) -> Result<String> {
    let out = std::process::Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(path)
        .arg("-")
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let t = tidy(&String::from_utf8_lossy(&o.stdout));
            if t.is_empty() {
                Ok("（PDF からテキストを抽出できませんでした。s で開いてください）".into())
            } else {
                Ok(t)
            }
        }
        Ok(o) => bail!("pdftotext が失敗しました: {}", String::from_utf8_lossy(&o.stderr).trim()),
        // pdftotext が無ければ組み込みの抽出器を使う
        Err(_) => {
            let bytes = std::fs::read(path)?;
            match std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes)) {
                // 行ごとに空行が挟まるので取り除く
                Ok(Ok(t)) if !t.trim().is_empty() => Ok(tidy(&t.replace("\n\n", "\n"))),
                Ok(Ok(_)) => Ok("（PDF からテキストを抽出できませんでした。s で開いてください）".into()),
                Ok(Err(e)) => bail!("PDF を解析できません: {e}（brew install poppler で改善する場合があります）"),
                Err(_) => bail!("PDF を解析できません（brew install poppler で改善する場合があります）"),
            }
        }
    }
}

/// 告示ファイルを dir に保存
pub fn download(store: &Store, e: &Entry, dir: &Path) -> Result<PathBuf> {
    let (_, bytes) = fetch_page(store, &e.rec.url)?;
    std::fs::create_dir_all(dir).with_context(|| format!("ディレクトリを作成できません: {}", dir.display()))?;
    let mut name = e.file_name();
    if bytes.starts_with(b"%PDF") && !name.ends_with(".pdf") {
        name.push_str(".pdf");
    }
    let path = dir.join(name);
    std::fs::write(&path, &bytes).with_context(|| format!("保存できません: {}", path.display()))?;
    Ok(path)
}

/// 既定のアプリで開く。PDF はキャッシュに取得してローカルファイルとして開く（macOS ならプレビュー.app）
pub fn show(store: &Store, e: &Entry) -> Result<()> {
    if e.ext().eq_ignore_ascii_case("pdf") {
        let (_, bytes) = fetch_page(store, &e.rec.url)?;
        if bytes.starts_with(b"%PDF") {
            // ビューアのタイトルが分かりやすいよう、告示番号入りのファイル名で開く
            let dir = store.dir.join("open");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(e.file_name());
            std::fs::write(&path, &bytes)?;
            return open::that_detached(&path).with_context(|| format!("開けません: {}", path.display()));
        }
    }
    open::that_detached(&e.rec.url).with_context(|| format!("開けません: {}", e.rec.url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_orgs() {
        assert_eq!(short_org("国土交通省住宅局建築指導課", ""), "住宅局");
        assert_eq!(short_org("建設省住宅局長通知", ""), "住宅局");
        assert_eq!(short_org("道路局長", ""), "道路局");
        assert_eq!(short_org("国土交通省", ""), "国土交通省");
        assert_eq!(short_org("運輸省", ""), "運輸省");
        assert_eq!(short_org("国土庁大都市圏整備局長通知", ""), "大都市圏整備局");
        assert_eq!(short_org("", "建設省告示第1446号"), "建設省");
        assert_eq!(short_org("", "国住指第12号"), "住宅局");
        assert_eq!(short_org("", "54住指発第3号"), "住宅局");
    }

    #[test]
    fn serial_dates() {
        assert_eq!(civil_from_serial(46308), (2026, 10, 13));
        assert_eq!(civil_from_serial(1), (1899, 12, 31));
        assert_eq!(era(2000, 5, 31).0, "H12.05.31");
        assert_eq!(era(2019, 5, 1).1, "令和1年5月1日");
    }

    fn entry(number: &str, title: &str) -> Entry {
        Entry::new(Record {
            title: title.into(),
            number: number.into(),
            date: Some(36677),
            date_raw: String::new(),
            org: "国土交通省".into(),
            url: "https://example.com/a.pdf".into(),
        })
    }

    #[test]
    fn search_number_variants() {
        let es = vec![
            entry("建設省告示第千二十四号", "a"),
            entry("国土交通省告示第一〇二四号", "b"),
            entry("国土交通省告示第１０２４号", "c"),
            entry("国土交通省告示第1024号", "d"),
            entry("国土交通省告示第10245号", "e"),
            entry("国土交通省告示第2024号", "f"),
        ];
        for q in ["1024", "千二十四", "１０２４", "一〇二四"] {
            assert_eq!(Query::parse(q).filter(&es), vec![0, 1, 2, 3], "{q}");
        }
        assert_eq!(Query::parse("1024 建設省").filter(&es), vec![0]);
        assert_eq!(Query::parse("1024 !建設省").filter(&es), vec![1, 2, 3]);
        assert_eq!(Query::parse("h12").filter(&es).len(), 6);
    }
}
