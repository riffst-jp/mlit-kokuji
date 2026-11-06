use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const APP: &str = "mlkokuji";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub download: DownloadConfig,
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// ウィンドウの高さ。"100%" で全画面、"40%" で端末の40%、"20" で20行。
    pub height: String,
    /// 起動時にプレビューを表示するか
    pub preview: bool,
    /// プレビューの幅（%）
    pub preview_width: u16,
    /// 起動時に検索入力モードで始めるか
    pub start_in_search: bool,
    /// 起動時に国交省サイトの更新を確認するか
    pub check_update: bool,
    /// 一覧に表示する列（左から順に）
    pub columns: Vec<Column>,
    /// 文書番号を半角英数字に整形して表示するか（false なら Excel の原文のまま）
    pub normalize_number: bool,
}

/// 一覧の列
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    /// 所轄（局レベルに短縮した組織名）
    Org,
    /// 組織名（Excel の原文）
    OrgFull,
    /// 文書番号
    Number,
    /// 日付 (YYYY-MM-DD)
    Date,
    /// 和暦 (H12.05.31)
    Era,
    /// タイトル
    Title,
}

impl Column {
    pub fn header(self) -> &'static str {
        match self {
            Column::Org => "所轄",
            Column::OrgFull => "組織名",
            Column::Number => "文書番号",
            Column::Date => "日付",
            Column::Era => "和暦",
            Column::Title => "タイトル",
        }
    }
}

pub fn default_columns() -> Vec<Column> {
    vec![Column::Org, Column::Number, Column::Date, Column::Title]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadConfig {
    /// `get` / `d` キーの保存先ディレクトリ
    pub dir: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// キャッシュディレクトリ。空なら $XDG_CACHE_HOME/mlkokuji
    pub dir: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            height: "100%".into(),
            preview: true,
            preview_width: 50,
            start_in_search: true,
            check_update: true,
            columns: default_columns(),
            normalize_number: true,
        }
    }
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self { dir: "~/Downloads".into() }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(p) if !p.is_empty() && Path::new(&p).is_absolute() => PathBuf::from(p),
        _ => home().join(fallback),
    }
}

pub fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(s)
    }
}

pub fn config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join(APP).join("config.toml")
}

/// 既定の設定ファイルの内容（コメント付き）
pub const DEFAULT_TOML: &str = r#"# mlkokuji 設定ファイル

[ui]
# インタラクティブウィンドウの高さ。"100%" で全画面、"40%" で端末の40%、"20" で20行
height = "100%"
# 起動時にプレビューを表示する（P で切り替え）
preview = true
# プレビューの幅（%）
preview_width = 50
# 起動時に検索入力（SEARCH）モードで始める。false なら NORMAL モードで始める
start_in_search = true
# 起動時に国交省サイトの更新を確認する
check_update = true
# 一覧に表示する列（左から順に）。`mlkokuji list` の出力にも使われる
#   "org"      所轄（局レベルに短縮した組織名。例: 住宅局）
#   "org_full" 組織名（原文。例: 国土交通省住宅局建築指導課）
#   "number"   文書番号
#   "date"     日付（2000-05-31）
#   "era"      和暦（H12.05.31）
#   "title"    タイトル
columns = ["org", "number", "date", "title"]
# 文書番号を半角英数字に整形して表示する（例: 建設省告示第千四百四十六号 → 建設省告示第1446号）
# false なら原文のまま表示する。検索はどちらの場合も表記揺れを吸収する
normalize_number = true

[download]
# `mlkokuji get` / `d` キーでの保存先
dir = "~/Downloads"

[cache]
# キャッシュディレクトリ。空なら $XDG_CACHE_HOME/mlkokuji (~/.cache/mlkokuji)
dir = ""
"#;

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(&path)
            .with_context(|| format!("設定ファイルを読めません: {}", path.display()))?;
        toml::from_str(&s).with_context(|| format!("設定ファイルの形式が不正です: {}", path.display()))
    }

    pub fn init_file() -> Result<PathBuf> {
        let path = config_path();
        if !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, DEFAULT_TOML)?;
        }
        Ok(path)
    }

    pub fn cache_dir(&self) -> PathBuf {
        if !self.cache.dir.is_empty() {
            return expand_tilde(&self.cache.dir);
        }
        if let Some(p) = std::env::var_os("MLKOKUJI_CACHE_DIR").filter(|p| !p.is_empty()) {
            return PathBuf::from(p);
        }
        xdg("XDG_CACHE_HOME", ".cache").join(APP)
    }

    pub fn download_dir(&self) -> PathBuf {
        expand_tilde(&self.download.dir)
    }
}

/// 高さ指定
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Height {
    Full,
    Percent(u16),
    Lines(u16),
}

impl Height {
    pub fn parse(s: &str) -> Height {
        let s = s.trim();
        if let Some(p) = s.strip_suffix('%') {
            match p.trim().parse::<u16>() {
                Ok(p) if p >= 100 => Height::Full,
                Ok(p) if p > 0 => Height::Percent(p),
                _ => Height::Full,
            }
        } else {
            match s.parse::<u16>() {
                Ok(n) if n > 0 => Height::Lines(n),
                _ => Height::Full,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_toml_matches_default() {
        let c: Config = toml::from_str(DEFAULT_TOML).unwrap();
        let d = Config::default();
        assert_eq!(c.ui.height, d.ui.height);
        assert_eq!(c.download.dir, d.download.dir);
        assert_eq!(c.ui.columns, d.ui.columns);
        assert_eq!(c.ui.normalize_number, d.ui.normalize_number);
    }

    #[test]
    fn height() {
        assert_eq!(Height::parse("100%"), Height::Full);
        assert_eq!(Height::parse("40%"), Height::Percent(40));
        assert_eq!(Height::parse("20"), Height::Lines(20));
    }
}
