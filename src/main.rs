mod config;
mod data;
mod kansuji;
mod tui;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

use config::Config;
use data::{Entry, Meta, Query, Store};

/// 国土交通省の告示・通達を検索・表示する CLI
///
/// 引数なしで起動するとインタラクティブに検索できます。
/// 告示番号は 1024 / １０２４ / 千二十四 / 一〇二四 のいずれの表記でも検索できます。
#[derive(Parser)]
#[command(name = "mlkokuji", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 告示を既定のアプリ（ブラウザ・PDF ビューア）で開く。複数該当すれば一覧から選択
    Show {
        /// 検索語（例: 1024, 1024 建設省）
        #[arg(required = true)]
        query: Vec<String>,
    },
    /// 告示をダウンロードする。複数該当すれば一覧から選択（g で保存）
    Get {
        /// 検索語（例: 1024, 1024 建設省）
        #[arg(required = true)]
        query: Vec<String>,
        /// 保存先ディレクトリ（既定: 設定ファイルの download.dir）
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// 該当するものをすべて保存する
        #[arg(short, long)]
        all: bool,
    },
    /// 一覧を表示する（タブ区切り。設定 ui.columns の列と URL）
    List {
        /// 検索語
        query: Vec<String>,
    },
    /// 国交省サイトから一覧データを再取得する
    Update,
    /// 設定ファイルの場所と内容を表示する
    Config {
        /// 既定の設定ファイルを作成する
        #[arg(long)]
        init: bool,
        /// $EDITOR で設定ファイルを開く
        #[arg(long)]
        edit: bool,
    },
    /// キャッシュを削除する
    Clean,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("mlkokuji: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;
    let store = Store::new(cfg.cache_dir());

    match cli.cmd {
        None => {
            let (meta, entries) = load_or_update(&cfg, &store)?;
            let dir = cfg.download_dir();
            tui::run(cfg, store, meta, entries, tui::Options { query: String::new(), download_dir: dir })
        }
        Some(Cmd::Show { query }) => {
            let (meta, entries) = load_or_update(&cfg, &store)?;
            let q = query.join(" ");
            let hits = Query::parse(&q).filter(&entries);
            match hits.as_slice() {
                [] => bail!("「{q}」に該当する告示はありません"),
                [i] => {
                    let e = &entries[*i];
                    println!("{}  {}  {}", e.shown_number, e.date, e.rec.title);
                    data::show(&store, e)
                }
                _ => pick(cfg, store, meta, entries, &hits, q, None),
            }
        }
        Some(Cmd::Get { query, output, all }) => {
            let (meta, entries) = load_or_update(&cfg, &store)?;
            let q = query.join(" ");
            let dir = output.unwrap_or_else(|| cfg.download_dir());
            let hits = Query::parse(&q).filter(&entries);
            if hits.is_empty() {
                bail!("「{q}」に該当する告示はありません");
            }
            if hits.len() == 1 || all {
                for &i in &hits {
                    let p = data::download(&store, &entries[i], &dir)?;
                    println!("{}", p.display());
                }
                return Ok(());
            }
            pick(cfg, store, meta, entries, &hits, q, Some(dir))
        }
        Some(Cmd::List { query }) => {
            let (_, entries) = load_or_update(&cfg, &store)?;
            let hits = Query::parse(&query.join(" ")).filter(&entries);
            let mut out = std::io::stdout().lock();
            for i in hits {
                let e = &entries[i];
                use std::io::Write;
                if writeln!(out, "{}", tsv_row(&cfg, e)).is_err() {
                    break; // パイプが閉じられた
                }
            }
            Ok(())
        }
        Some(Cmd::Update) => {
            let (meta, entries) = update(&store)?;
            println!("{} 件（サイト最終更新日: {}）", entries.len(), meta.site_updated);
            Ok(())
        }
        Some(Cmd::Config { init, edit }) => {
            let path = if init || edit { Config::init_file()? } else { config::config_path() };
            if edit {
                let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
                let st = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("{editor} \"$1\""))
                    .arg("sh")
                    .arg(&path)
                    .status()?;
                if !st.success() {
                    bail!("エディタが異常終了しました");
                }
                Config::load()?;
                return Ok(());
            }
            println!("# {}", path.display());
            if path.exists() {
                print!("{}", std::fs::read_to_string(&path)?);
            } else {
                println!("# （未作成。`mlkokuji config --init` で作成できます。既定値:）");
                print!("{}", config::DEFAULT_TOML);
            }
            println!("\n# cache: {}", store.dir.display());
            Ok(())
        }
        Some(Cmd::Clean) => {
            store.clean()?;
            println!("キャッシュを削除しました: {}", store.dir.display());
            Ok(())
        }
    }
}

fn update(store: &Store) -> Result<(Meta, Vec<Entry>)> {
    eprintln!("国交省サイトから告示一覧を取得しています… ({})", data::INDEX_URL);
    store.update(None)
}

fn load_or_update(cfg: &Config, store: &Store) -> Result<(Meta, Vec<Entry>)> {
    let (meta, mut entries) = if store.exists() { store.load()? } else { update(store)? };
    data::set_number_format(&mut entries, cfg.ui.normalize_number);
    Ok((meta, entries))
}

/// タブ区切りの 1 行（設定の列 + URL）
fn tsv_row(cfg: &Config, e: &Entry) -> String {
    let cols = if cfg.ui.columns.is_empty() { config::default_columns() } else { cfg.ui.columns.clone() };
    let mut v: Vec<&str> = cols.iter().map(|&c| e.column(c)).collect();
    v.push(&e.rec.url);
    v.join("\t")
}

/// 複数該当時: 端末ならインタラクティブ選択、そうでなければ一覧を出力
fn pick(
    cfg: Config,
    store: Store,
    meta: Meta,
    entries: Vec<Entry>,
    hits: &[usize],
    query: String,
    dir: Option<PathBuf>,
) -> Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        for &i in hits {
            let e = &entries[i];
            println!("{}", tsv_row(&cfg, e));
        }
        bail!("{} 件該当しました。検索語を追加して絞り込んでください", hits.len());
    }
    let download_dir = dir.unwrap_or_else(|| cfg.download_dir());
    tui::run(cfg, store, meta, entries, tui::Options { query, download_dir })
}
