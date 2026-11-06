use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame, TerminalOptions, Viewport};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{Column, Config, Height, default_columns};
use crate::data::{self, Entry, Meta, Query, Store};

enum Msg {
    Preview(String, Result<String, String>),
    Checked(Result<Option<Meta>, String>),
    Updated(Result<(Meta, Vec<Entry>), String>),
    Downloaded(Result<PathBuf, String>),
    Opened(Result<String, String>),
}

#[derive(PartialEq)]
enum Mode {
    Normal,
    Search,
}

enum UpdateState {
    Idle,
    Checking,
    Available(Meta),
    Updating,
}

enum Preview {
    Loading,
    Ready(Arc<String>),
    Failed(String),
}

pub struct Options {
    pub query: String,
    pub download_dir: PathBuf,
}

struct App {
    cfg: Config,
    store: Arc<Store>,
    meta: Meta,
    entries: Vec<Entry>,
    filtered: Vec<usize>,
    table: TableState,
    query: Vec<char>,
    cursor: usize,
    mode: Mode,
    show_preview: bool,
    previews: HashMap<String, Preview>,
    preview_scroll: u16,
    selected_since: Instant,
    update: UpdateState,
    status: Option<(String, Instant)>,
    help: bool,
    /// `gg` の 1 打目を受け付けた状態
    pending_g: bool,
    list_height: usize,
    download_dir: PathBuf,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
}

pub fn run(cfg: Config, store: Store, meta: Meta, entries: Vec<Entry>, opts: Options) -> Result<()> {
    let (tx, rx) = channel();
    let mut app = App {
        show_preview: cfg.ui.preview,
        mode: if cfg.ui.start_in_search { Mode::Search } else { Mode::Normal },
        store: Arc::new(store),
        meta,
        entries,
        filtered: Vec::new(),
        table: TableState::default(),
        cursor: opts.query.chars().count(),
        query: opts.query.chars().collect(),
        previews: HashMap::new(),
        preview_scroll: 0,
        selected_since: Instant::now(),
        update: UpdateState::Idle,
        status: None,
        help: false,
        pending_g: false,
        list_height: 10,
        download_dir: opts.download_dir,
        tx,
        rx,
        quit: false,
        cfg,
    };
    app.refilter();
    if app.cfg.ui.check_update {
        app.spawn_check();
    }

    let height = Height::parse(&app.cfg.ui.height);
    match height {
        Height::Full => {
            let mut term = ratatui::init();
            quiet_worker_panics();
            let r = app.event_loop(&mut term);
            ratatui::restore();
            r
        }
        Height::Percent(_) | Height::Lines(_) => {
            let (_, rows) = terminal::size()?;
            let h = match height {
                Height::Percent(p) => (rows as u32 * p as u32 / 100) as u16,
                Height::Lines(n) => n,
                Height::Full => rows,
            }
            .clamp(6, rows.max(6));
            let mut term = ratatui::init_with_options(TerminalOptions { viewport: Viewport::Inline(h) });
            quiet_worker_panics();
            let r = app.event_loop(&mut term);
            let _ = term.clear();
            ratatui::restore();
            r
        }
    }
}

/// バックグラウンドスレッドのパニックで画面が乱れないよう、worker スレッドでは出力しない
fn quiet_worker_panics() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some(WORKER) {
            prev(info);
        }
    }));
}

const WORKER: &str = "mlkokuji-worker";

fn spawn(f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(WORKER.into()).spawn(f);
}

impl App {
    fn event_loop(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                    _ => {}
                }
            }
            while let Ok(m) = self.rx.try_recv() {
                self.on_msg(m);
            }
            self.maybe_load_preview();
        }
        Ok(())
    }

    // -- 状態操作 ---------------------------------------------------------

    fn query_string(&self) -> String {
        self.query.iter().collect()
    }

    fn current(&self) -> Option<&Entry> {
        self.table.selected().and_then(|i| self.filtered.get(i)).map(|&i| &self.entries[i])
    }

    fn refilter(&mut self) {
        let cur_url = self.current().map(|e| e.rec.url.clone());
        self.filtered = Query::parse(&self.query_string()).filter(&self.entries);
        let idx = cur_url
            .and_then(|u| self.filtered.iter().position(|&i| self.entries[i].rec.url == u))
            .unwrap_or(0);
        self.select(if self.filtered.is_empty() { None } else { Some(idx) });
    }

    fn select(&mut self, i: Option<usize>) {
        if i != self.table.selected() {
            self.preview_scroll = 0;
            self.selected_since = Instant::now();
        }
        self.table.select(i);
    }

    fn move_by(&mut self, d: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() as isize - 1;
        let cur = self.table.selected().unwrap_or(0) as isize;
        self.select(Some((cur + d).clamp(0, max) as usize));
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    // -- バックグラウンド処理 --------------------------------------------

    fn spawn_check(&mut self) {
        self.update = UpdateState::Checking;
        let tx = self.tx.clone();
        let meta = self.meta.clone();
        spawn(move || {
            let _ = tx.send(Msg::Checked(data::check_update(&meta).map_err(|e| format!("{e:#}"))));
        });
    }

    fn spawn_update(&mut self) {
        if matches!(self.update, UpdateState::Updating) {
            return;
        }
        let remote = match &self.update {
            UpdateState::Available(m) => Some(m.clone()),
            _ => None,
        };
        self.update = UpdateState::Updating;
        self.set_status("国交省サイトからデータを取得しています…");
        let tx = self.tx.clone();
        let store = self.store.clone();
        spawn(move || {
            let _ = tx.send(Msg::Updated(store.update(remote).map_err(|e| format!("{e:#}"))));
        });
    }

    fn spawn_download(&mut self) {
        let Some(e) = self.current().cloned() else { return };
        self.set_status(format!("ダウンロード中: {}", e.shown_number));
        let tx = self.tx.clone();
        let store = self.store.clone();
        let dir = self.download_dir.clone();
        spawn(move || {
            let _ = tx.send(Msg::Downloaded(data::download(&store, &e, &dir).map_err(|e| format!("{e:#}"))));
        });
    }

    fn spawn_open(&mut self) {
        let Some(e) = self.current().cloned() else { return };
        self.set_status(format!("開いています: {}", e.shown_number));
        let tx = self.tx.clone();
        let store = self.store.clone();
        spawn(move || {
            let r = data::show(&store, &e).map(|_| format!("開きました: {}", e.rec.url));
            let _ = tx.send(Msg::Opened(r.map_err(|e| format!("{e:#}"))));
        });
    }

    fn maybe_load_preview(&mut self) {
        if !self.show_preview || self.selected_since.elapsed() < Duration::from_millis(120) {
            return;
        }
        let Some(e) = self.current().cloned() else { return };
        if self.previews.contains_key(&e.rec.url) {
            return;
        }
        self.previews.insert(e.rec.url.clone(), Preview::Loading);
        let tx = self.tx.clone();
        let store = self.store.clone();
        spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| data::preview_text(&store, &e)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("プレビューの生成中にエラーが発生しました")))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Preview(e.rec.url.clone(), r));
        });
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Preview(url, r) => {
                let p = match r {
                    Ok(t) => Preview::Ready(Arc::new(t)),
                    Err(e) => Preview::Failed(e),
                };
                self.previews.insert(url, p);
            }
            Msg::Checked(r) => {
                if matches!(self.update, UpdateState::Checking) {
                    self.update = match r {
                        Ok(Some(m)) => UpdateState::Available(m),
                        Ok(None) => UpdateState::Idle,
                        Err(e) => {
                            self.set_status(format!("更新確認に失敗: {e}"));
                            UpdateState::Idle
                        }
                    };
                }
            }
            Msg::Updated(r) => {
                self.update = UpdateState::Idle;
                match r {
                    Ok((meta, mut entries)) => {
                        data::set_number_format(&mut entries, self.cfg.ui.normalize_number);
                        let n = entries.len();
                        self.meta = meta;
                        self.entries = entries;
                        self.refilter();
                        self.set_status(format!("更新しました: {n} 件（{}）", self.meta.site_updated));
                    }
                    Err(e) => self.set_status(format!("更新に失敗: {e}")),
                }
            }
            Msg::Downloaded(r) => match r {
                Ok(p) => self.set_status(format!("保存しました: {}", p.display())),
                Err(e) => self.set_status(format!("保存に失敗: {e}")),
            },
            Msg::Opened(r) => match r {
                Ok(s) => self.set_status(s),
                Err(e) => self.set_status(format!("開けません: {e}")),
            },
        }
    }

    // -- キー入力 ---------------------------------------------------------

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if self.help {
            self.help = false;
            return;
        }
        // 両モード共通
        let page = self.list_height.max(1) as isize;
        match (k.code, ctrl, alt) {
            (KeyCode::Char('c'), true, _) => return self.quit = true,
            (KeyCode::Down, ..) | (KeyCode::Char('n'), true, _) => return self.move_by(1),
            (KeyCode::Up, ..) | (KeyCode::Char('p'), true, _) => return self.move_by(-1),
            (KeyCode::PageDown, ..) | (KeyCode::Char('v'), true, _) => return self.move_by(page),
            (KeyCode::PageUp, ..) | (KeyCode::Char('v'), false, true) => return self.move_by(-page),
            (KeyCode::Char('<'), false, true) => return self.move_by(isize::MIN / 2),
            (KeyCode::Char('>'), false, true) => return self.move_by(isize::MAX / 2),
            _ => {}
        }
        match self.mode {
            Mode::Normal => self.on_key_normal(k, ctrl, page),
            Mode::Search => self.on_key_search(k, ctrl, alt),
        }
    }

    fn on_key_normal(&mut self, k: KeyEvent, ctrl: bool, page: isize) {
        let pending_g = std::mem::take(&mut self.pending_g);
        match (k.code, ctrl) {
            (KeyCode::Char('g'), false) if pending_g => self.move_by(isize::MIN / 2),
            (KeyCode::Char('g'), false) => self.pending_g = true,
            (KeyCode::Char('q'), false) | (KeyCode::Esc, _) | (KeyCode::Char('g'), true) => self.quit = true,
            (KeyCode::Char('j'), false) => self.move_by(1),
            (KeyCode::Char('k'), false) => self.move_by(-1),
            (KeyCode::Char('d'), true) => self.move_by(page / 2),
            (KeyCode::Char('u'), true) => self.move_by(-page / 2),
            (KeyCode::Char('f'), true) => self.move_by(page),
            (KeyCode::Char('b'), true) => self.move_by(-page),
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => self.move_by(isize::MAX / 2),
            (KeyCode::Home, _) => self.move_by(isize::MIN / 2),
            (KeyCode::Char('J'), false) | (KeyCode::Char('e'), true) => {
                self.preview_scroll = self.preview_scroll.saturating_add(1)
            }
            (KeyCode::Char('K'), false) | (KeyCode::Char('y'), true) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1)
            }
            (KeyCode::Char(' '), false) => self.preview_scroll = self.preview_scroll.saturating_add(10),
            (KeyCode::Backspace, _) => self.preview_scroll = self.preview_scroll.saturating_sub(10),
            (KeyCode::Char('/'), false) | (KeyCode::Char('i'), false) | (KeyCode::Char('s'), true) => {
                self.mode = Mode::Search;
                self.cursor = self.query.len();
            }
            (KeyCode::Enter, _) => self.spawn_open(),
            (KeyCode::Char('d'), false) => self.spawn_download(),
            (KeyCode::Char('P'), false) => self.show_preview = !self.show_preview,
            (KeyCode::Char('u'), false) => self.spawn_update(),
            (KeyCode::Char('?'), false) => self.help = true,
            (KeyCode::Char('x'), false) => {
                self.query.clear();
                self.cursor = 0;
                self.refilter();
            }
            _ => {}
        }
    }

    fn on_key_search(&mut self, k: KeyEvent, ctrl: bool, alt: bool) {
        let before = self.query.clone();
        match (k.code, ctrl, alt) {
            (KeyCode::Esc, ..) | (KeyCode::Enter, ..) | (KeyCode::Char('g'), true, _) | (KeyCode::Char('['), true, _) => {
                self.mode = Mode::Normal
            }
            (KeyCode::Left, ..) | (KeyCode::Char('b'), true, _) => self.cursor = self.cursor.saturating_sub(1),
            (KeyCode::Right, ..) | (KeyCode::Char('f'), true, _) => self.cursor = (self.cursor + 1).min(self.query.len()),
            (KeyCode::Home, ..) | (KeyCode::Char('a'), true, _) => self.cursor = 0,
            (KeyCode::End, ..) | (KeyCode::Char('e'), true, _) => self.cursor = self.query.len(),
            (KeyCode::Char('b'), false, true) => self.cursor = self.word_left(),
            (KeyCode::Char('f'), false, true) => self.cursor = self.word_right(),
            (KeyCode::Backspace, _, false) | (KeyCode::Char('h'), true, _) => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.query.remove(self.cursor);
                }
            }
            (KeyCode::Delete, ..) | (KeyCode::Char('d'), true, _) => {
                if self.cursor < self.query.len() {
                    self.query.remove(self.cursor);
                }
            }
            (KeyCode::Char('w'), true, _) | (KeyCode::Backspace, _, true) => {
                let s = self.word_left();
                self.query.drain(s..self.cursor);
                self.cursor = s;
            }
            (KeyCode::Char('d'), false, true) => {
                let e = self.word_right();
                self.query.drain(self.cursor..e);
            }
            (KeyCode::Char('u'), true, _) => {
                self.query.drain(..self.cursor);
                self.cursor = 0;
            }
            (KeyCode::Char('k'), true, _) => self.query.truncate(self.cursor),
            (KeyCode::Char('j'), true, _) => self.move_by(1),
            (KeyCode::Char(c), false, false) => {
                self.query.insert(self.cursor, c);
                self.cursor += 1;
            }
            _ => {}
        }
        if self.query != before {
            self.refilter();
        }
    }

    fn word_left(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.query[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.query[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let mut i = self.cursor;
        let n = self.query.len();
        while i < n && self.query[i].is_whitespace() {
            i += 1;
        }
        while i < n && !self.query[i].is_whitespace() {
            i += 1;
        }
        i
    }

    // -- 描画 -------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [top, main, bottom] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        self.draw_prompt(f, top);

        let (list_area, preview_area) = if self.show_preview {
            let w = self.cfg.ui.preview_width.clamp(10, 90);
            let [l, p] =
                Layout::horizontal([Constraint::Percentage(100 - w), Constraint::Percentage(w)]).areas(main);
            (l, Some(p))
        } else {
            (main, None)
        };
        self.draw_list(f, list_area);
        if let Some(p) = preview_area {
            self.draw_preview(f, p);
        }
        self.draw_status(f, bottom);
        if self.help {
            draw_help(f, main);
        }
    }

    fn draw_prompt(&self, f: &mut Frame, area: Rect) {
        let searching = self.mode == Mode::Search;
        let prompt = if searching { Span::styled("/ ", Style::new().fg(Color::Yellow).bold()) } else { Span::styled("> ", Style::new().fg(Color::Cyan).bold()) };
        let q = self.query_string();
        let count = format!(" {}/{} ", self.filtered.len(), self.entries.len());
        let badge = match &self.update {
            UpdateState::Available(m) => Span::styled(
                format!(" Available update{} (u) ", if m.site_updated.is_empty() { String::new() } else { format!(": {}", m.site_updated) }),
                Style::new().fg(Color::Black).bg(Color::Yellow).bold(),
            ),
            UpdateState::Updating => Span::styled(" Updating… ", Style::new().fg(Color::Black).bg(Color::Cyan)),
            UpdateState::Checking => Span::styled(" checking… ", Style::new().fg(Color::DarkGray)),
            UpdateState::Idle => Span::styled(
                if self.meta.site_updated.is_empty() { String::new() } else { format!(" {} ", self.meta.site_updated) },
                Style::new().fg(Color::DarkGray),
            ),
        };
        let right = Line::from(vec![Span::styled(count, Style::new().fg(Color::DarkGray)), badge]);
        let rw = right.width() as u16;
        let [l, r] = Layout::horizontal([Constraint::Min(1), Constraint::Length(rw)]).areas(area);
        let placeholder = if q.is_empty() {
            let hint = if searching {
                "検索語を入力（例: 1024 / 千二十四 建設省 / !通達）  Esc でキー操作"
            } else {
                "/ で検索（例: 1024 / 千二十四 建設省 / !通達）  ? でヘルプ"
            };
            Span::styled(hint, Style::new().fg(Color::DarkGray))
        } else {
            Span::raw(q.clone())
        };
        f.render_widget(Line::from(vec![prompt, placeholder]), l);
        f.render_widget(right, r);
        if searching {
            let w: usize = self.query[..self.cursor].iter().map(|c| ratatui::text::Span::raw(c.to_string()).width()).sum();
            f.set_cursor_position((l.x + 2 + w as u16, l.y));
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        self.list_height = area.height.saturating_sub(2) as usize;
        let cols: Vec<Column> = if self.cfg.ui.columns.is_empty() { default_columns() } else { self.cfg.ui.columns.clone() };
        let color = |c: Column| match c {
            Column::Org | Column::OrgFull => Style::new().fg(Color::Magenta),
            Column::Number => Style::new().fg(Color::Cyan),
            Column::Date | Column::Era => Style::new().fg(Color::Green),
            Column::Title => Style::new(),
        };
        let rows: Vec<Row> = self
            .filtered
            .iter()
            .map(|&i| {
                let e = &self.entries[i];
                Row::new(cols.iter().map(|&c| Cell::from(e.column(c).to_string()).style(color(c))))
            })
            .collect();
        // タイトル列（無ければ最後の列）が残り幅を使い、他は内容に合わせた幅
        let fill = cols.iter().position(|&c| c == Column::Title).unwrap_or(cols.len() - 1);
        let mut fixed: Vec<usize> = cols
            .iter()
            .enumerate()
            .map(|(n, &c)| {
                if n == fill {
                    return 0;
                }
                let (lo, hi) = match c {
                    Column::Org => (4, 14),
                    Column::OrgFull => (6, 30),
                    Column::Number => (8, 32),
                    Column::Date => (10, 10),
                    Column::Era => (9, 9),
                    Column::Title => (10, 40),
                };
                self.filtered
                    .iter()
                    .take(500)
                    .map(|&i| Span::raw(self.entries[i].column(c)).width())
                    .chain([Span::raw(c.header()).width()])
                    .max()
                    .unwrap_or(lo)
                    .clamp(lo, hi)
            })
            .collect();
        // 伸縮する列に最低幅を確保し、足りなければ最も広い固定列から削る
        let avail = (area.width as usize).saturating_sub(2 + 1 + cols.len().saturating_sub(1));
        let budget = avail.saturating_sub(16.min(avail / 2));
        while fixed.iter().sum::<usize>() > budget {
            let (i, &w) = fixed.iter().enumerate().max_by_key(|&(_, w)| *w).unwrap();
            if w <= 4 {
                break;
            }
            fixed[i] -= 1;
        }
        let widths: Vec<Constraint> = fixed
            .iter()
            .enumerate()
            .map(|(n, &w)| if n == fill { Constraint::Fill(1) } else { Constraint::Length(w as u16) })
            .collect();
        let table = Table::new(rows, widths)
            .header(
                Row::new(cols.iter().map(|c| c.header()))
                    .style(Style::new().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
            )
            .block(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(" 告示・通達 "))
            .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌")
            .column_spacing(1);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let Some(e) = self.current() else {
            f.render_widget(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded), area);
            return;
        };
        let title = format!(" {} ", e.rec.title);
        let mut head = vec![
            Line::from(vec![Span::styled(e.shown_number.clone(), Style::new().fg(Color::Cyan).bold())]),
            Line::from(vec![
                Span::styled(format!("{}  {}", e.date, e.era), Style::new().fg(Color::Green)),
                Span::raw("  "),
                Span::styled(e.rec.org.clone(), Style::new().fg(Color::Magenta)),
            ]),
            Line::from(Span::styled(e.rec.url.clone(), Style::new().fg(Color::DarkGray))),
            Line::raw(""),
        ];
        let body: Vec<Line> = match self.previews.get(&e.rec.url) {
            None | Some(Preview::Loading) => vec![Line::styled("読み込み中…", Style::new().fg(Color::DarkGray))],
            Some(Preview::Failed(err)) => vec![Line::styled(err.clone(), Style::new().fg(Color::Red))],
            Some(Preview::Ready(t)) => t.lines().map(|l| Line::raw(l.to_string())).collect(),
        };
        head.extend(body);
        let p = Paragraph::new(head)
            .block(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.preview_scroll, 0));
        f.render_widget(p, area);
    }

    fn draw_status(&mut self, f: &mut Frame, area: Rect) {
        if let Some((_, t)) = &self.status
            && t.elapsed() > Duration::from_secs(8) {
                self.status = None;
            }
        let line = match &self.status {
            Some((s, _)) => Line::styled(s.clone(), Style::new().fg(Color::Yellow)),
            None => {
                let mode = match self.mode {
                    Mode::Normal => Span::styled(" NORMAL ", Style::new().fg(Color::Black).bg(Color::Cyan)),
                    Mode::Search => Span::styled(" SEARCH ", Style::new().fg(Color::Black).bg(Color::Yellow)),
                };
                let keys = match self.mode {
                    Mode::Normal => " Enter:開く  d:保存  P:プレビュー  u:更新  /:検索  J/K:本文スクロール  ?:ヘルプ  q:終了",
                    Mode::Search => " Enter/Esc:確定  C-n/C-p:移動  C-a/C-e/C-w/C-u/C-k:編集",
                };
                Line::from(vec![mode, Span::styled(keys, Style::new().fg(Color::DarkGray))])
            }
        };
        f.render_widget(line, area);
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let text = "\
 NORMAL モード
   j / k / ↓ / ↑ / C-n / C-p   上下に移動
   C-d / C-u                   半ページ移動
   C-f / C-b / C-v / M-v       1ページ移動
   G / End / M->               末尾へ
   gg / Home / M-<             先頭へ
   J / K / C-e / C-y           プレビューを1行スクロール
   Space / Backspace           プレビューを10行スクロール
   Enter                       告示を開く（PDF はプレビュー.app 等）
   d                           ダウンロード
   P                           プレビュー表示の切り替え
   u                           国交省サイトからデータ再取得
   / / i / C-s                 検索入力
   x                           検索語をクリア
   q / Esc / C-g / C-c         終了

 SEARCH モード
   Enter / Esc / C-g           検索を確定して NORMAL へ
   C-n / C-p / ↓ / ↑           上下に移動
   C-a / C-e / C-b / C-f       カーソル移動 (M-b / M-f で単語)
   C-h / C-d / C-w / C-u / C-k 削除

 検索語
   空白区切りで AND 検索。数字のみの語は文書番号の数字と完全一致
   （1024 / １０２４ / 千二十四 / 一〇二四 はすべて同じ）。
   !語 で除外。和暦 (h12, 平成12年) でも検索可。

 何かキーを押すと閉じます";
    let h = (text.lines().count() as u16 + 2).min(area.height);
    let w = 72.min(area.width);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text).block(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded).title(" ヘルプ ")),
        r,
    );
}
