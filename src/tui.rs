//! Human mail client: a keyboard-driven TUI (ratatui + crossterm).
//!
//! Three panes: thread list, conversation view, and a status bar; modal
//! dialogs for composing, the allowlist editor, message details, and help.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use iroh::SecretKey;
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::allowlist::{AllowList, PeerEntry};
use crate::config::{Config, Paths};
use crate::identity;
use crate::ops;
use crate::store::{Rejection, Store, StoredMessage, ThreadSummary};

const REFRESH: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Threads,
    Messages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeField {
    Peer,
    Body,
}

#[derive(Debug)]
struct Compose {
    peer: String,
    body: String,
    field: ComposeField,
    reply_to: Option<String>, // msg_key being replied to
    status: String,
}

impl Compose {
    fn new(peer: impl Into<String>, reply_to: Option<String>) -> Self {
        Self {
            peer: peer.into(),
            body: String::new(),
            field: ComposeField::Peer,
            reply_to,
            status: String::new(),
        }
    }
}

#[derive(Debug)]
enum Modal {
    Compose(Compose),
    AllowList {
        entries: Vec<PeerEntry>,
        rejections: Vec<Rejection>,
        selected: usize,
        input: Option<String>, // Some = typing a new NodeId
    },
    Raw(StoredMessage),
    Help,
}

pub struct App {
    paths: Paths,
    config: Config,
    secret_key: SecretKey,
    me: String,
    store: Store,
    threads: Vec<ThreadSummary>,
    messages: Vec<StoredMessage>,
    selected_thread: usize,
    selected_msg: usize,
    focus: Focus,
    backlog: u64,
    outbox_failed: u64,
    modal: Option<Modal>,
    peer_names: Vec<(String, String)>, // (node_id, name) for display + completion
    last_status: String,
    should_quit: bool,
}

impl App {
    fn new(paths: Paths, config: Config) -> Result<Self> {
        let secret_key = identity::load(&paths)?;
        let me = identity::node_id(&secret_key);
        let store = Store::open(&paths)?;
        let mut app = Self {
            paths,
            config,
            secret_key,
            me,
            store,
            threads: Vec::new(),
            messages: Vec::new(),
            selected_thread: 0,
            selected_msg: 0,
            focus: Focus::Threads,
            backlog: 0,
            outbox_failed: 0,
            modal: None,
            peer_names: Vec::new(),
            last_status: String::new(),
            should_quit: false,
        };
        app.refresh()?;
        Ok(app)
    }

    fn peer_display(&self, node_id: &str) -> String {
        self.peer_names
            .iter()
            .find(|(id, _)| id == node_id)
            .map(|(_, name)| format!("{name} ({})", short(node_id)))
            .unwrap_or_else(|| short(node_id).to_string())
    }

    fn refresh(&mut self) -> Result<()> {
        let list = AllowList::load(&self.paths.allowed_keys)?;
        self.peer_names = list
            .entries
            .iter()
            .map(|e| (e.node_id.clone(), e.name.clone().unwrap_or_default()))
            .collect();
        self.threads = self.store.list_threads()?;
        self.selected_thread = self.selected_thread.min(self.threads.len().saturating_sub(1));
        self.load_messages()?;
        let (pending, failed) = self.store.outbox_counts();
        self.backlog = pending;
        self.outbox_failed = failed;
        Ok(())
    }

    fn load_messages(&mut self) -> Result<()> {
        if let Some(t) = self.threads.get(self.selected_thread) {
            self.messages = self.store.thread_messages(&t.thread_id)?;
        } else {
            self.messages = Vec::new();
        }
        self.selected_msg = self
            .selected_msg
            .min(self.messages.len().saturating_sub(1));
        Ok(())
    }

    /// Deliberately open a thread: only then is it marked read. Merely
    /// moving the selection must not destroy the unread signal the agent
    /// relies on.
    fn open_thread(&mut self) -> Result<()> {
        if let Some(t) = self.threads.get(self.selected_thread) {
            let thread_id = t.thread_id.clone();
            self.store.mark_thread_read(&thread_id)?;
            self.load_messages()?;
            if let Some(t) = self.threads.get_mut(self.selected_thread) {
                t.unread_count = 0;
            }
        }
        Ok(())
    }

    async fn send_compose(&mut self, c: &Compose) {
        let outcome = async {
            let peer_id = AllowList::load(&self.paths.allowed_keys)?
                .resolve(&c.peer)
                .map(|e| e.node_id.clone())
                .ok_or_else(|| anyhow::anyhow!("`{}` is not in the allowlist", c.peer));
            let peer_id = peer_id?;
            let (thread, reply_to) = match &c.reply_to {
                Some(key) => {
                    let (_, thread, remote) = ops::reply_context(&self.paths, key)?;
                    (Some(thread), Some(remote))
                }
                None => (None, None),
            };
            ops::send_message(
                &self.paths,
                &self.config,
                &self.secret_key,
                &self.me,
                &peer_id,
                &c.body,
                Vec::new(),
                thread.as_deref(),
                reply_to.as_deref(),
            )
            .await
        }
        .await;

        match outcome {
            Ok(ops::DeliveryOutcome::Delivered { msg_key, .. }) => {
                self.status(format!("delivered ({})", short(&msg_key)));
            }
            Ok(ops::DeliveryOutcome::Queued { msg_key, .. }) => {
                self.status(format!("queued for retry ({})", short(&msg_key)));
            }
            Err(e) => self.status(format!("send failed: {e:#}")),
        }
        let _ = self.refresh();
    }

    fn status(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.last_status = msg.clone();
        if let Some(Modal::Compose(c)) = &mut self.modal {
            c.status = msg;
        }
    }
}

fn short(s: &str) -> &str {
    if s.len() > 16 { &s[..16] } else { s }
}

fn age(secs_ago: u64) -> String {
    if secs_ago < 60 {
        format!("{secs_ago}s")
    } else if secs_ago < 3600 {
        format!("{}m", secs_ago / 60)
    } else if secs_ago < 86400 {
        format!("{}h", secs_ago / 3600)
    } else {
        format!("{}d", secs_ago / 86400)
    }
}

pub async fn run(paths: Paths, config: Config) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(paths, config)?;
    let result = app.loop_(&mut terminal).await;
    ratatui::restore();
    result
}

impl App {
    async fn loop_(&mut self, terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
        let mut last_refresh = std::time::Instant::now() - REFRESH;
        while !self.should_quit {
            if last_refresh.elapsed() >= REFRESH {
                let _ = self.refresh();
                last_refresh = std::time::Instant::now();
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(REFRESH)? {
                if let Event::Key(key) = event::read()? {
                    self.on_key(key).await;
                }
            }
        }
        Ok(())
    }

    async fn on_key(&mut self, key: KeyEvent) {
        // modal handling first (no borrow held across the call)
        if matches!(self.modal, Some(Modal::Compose(_))) {
            self.on_compose_key(key).await;
            return;
        }
        if matches!(self.modal, Some(Modal::AllowList { .. })) {
            self.on_allow_key(key).await;
            return;
        }
        if matches!(self.modal, Some(Modal::Raw(_)) | Some(Modal::Help)) {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                _ => {}
            }
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => self.should_quit = true,
            (KeyCode::Tab, _) => {
                self.focus = match self.focus {
                    Focus::Threads => Focus::Messages,
                    Focus::Messages => Focus::Threads,
                }
            }
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => self.move_selection(1),
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => self.move_selection(-1),
            (KeyCode::Char('g'), _) => {
                let _ = self.refresh();
            }
            (KeyCode::Enter, _) if self.focus == Focus::Threads => {
                let _ = self.open_thread();
            }
            (KeyCode::Char('c'), _) => {
                let peer = self
                    .threads
                    .get(self.selected_thread)
                    .map(|t| t.peer.clone())
                    .unwrap_or_default();
                self.modal = Some(Modal::Compose(Compose::new(peer, None)));
            }
            (KeyCode::Char('r'), _) => {
                if let Some(m) = self.messages.last() {
                    self.modal = Some(Modal::Compose(Compose::new(
                        m.peer.clone(),
                        Some(m.msg_key.clone()),
                    )));
                }
            }
            (KeyCode::Char('v'), _) => {
                if let Some(m) = self.messages.get(self.selected_msg).cloned() {
                    self.modal = Some(Modal::Raw(m));
                }
            }
            (KeyCode::Char('a'), _) => {
                let entries = AllowList::load(&self.paths.allowed_keys)
                    .map(|l| l.entries)
                    .unwrap_or_default();
                let rejections = self.store.list_rejections().unwrap_or_default();
                self.modal = Some(Modal::AllowList {
                    entries,
                    rejections,
                    selected: 0,
                    input: None,
                });
            }
            (KeyCode::Char('?'), _) => self.modal = Some(Modal::Help),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Threads => {
                let n = self.threads.len() as i32;
                if n > 0 {
                    let next = (self.selected_thread as i32 + delta).clamp(0, n - 1);
                    if next != self.selected_thread as i32 {
                        self.selected_thread = next as usize;
                        let _ = self.load_messages();
                    }
                }
            }
            Focus::Messages => {
                let n = self.messages.len() as i32;
                if n > 0 {
                    let next = (self.selected_msg as i32 + delta).clamp(0, n - 1);
                    self.selected_msg = next as usize;
                }
            }
        }
    }
}

impl App {
    async fn on_compose_key(&mut self, key: KeyEvent) {
        let mut done: Option<Compose> = None;
        let mut close = false;
        if let Some(Modal::Compose(c)) = &mut self.modal {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => {
                    close = true;
                }
                (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                    c.field = match c.field {
                        ComposeField::Peer => ComposeField::Body,
                        ComposeField::Body => ComposeField::Peer,
                    };
                }
                (KeyCode::Enter, m) | (KeyCode::Char('s'), m)
                    if m.contains(KeyModifiers::CONTROL) =>
                {
                    if c.body.trim().is_empty() || c.peer.trim().is_empty() {
                        c.status = "peer and body are required".into();
                    } else {
                        done = Some(Compose {
                            peer: c.peer.clone(),
                            body: c.body.clone(),
                            field: c.field,
                            reply_to: c.reply_to.clone(),
                            status: c.status.clone(),
                        });
                    }
                }
                (KeyCode::Enter, _) if c.field == ComposeField::Body => {
                    c.body.push('\n');
                }
                (KeyCode::Enter, _) => {
                    c.field = ComposeField::Body;
                }
                (KeyCode::Backspace, _) => match c.field {
                    ComposeField::Peer => {
                        c.peer.pop();
                    }
                    ComposeField::Body => {
                        c.body.pop();
                    }
                },
                (KeyCode::Char(ch), _) => match c.field {
                    ComposeField::Peer => c.peer.push(ch),
                    ComposeField::Body => c.body.push(ch),
                },
                _ => {}
            }
        }
        if close {
            self.modal = None;
        }
        if let Some(c) = done {
            self.modal = None;
            self.send_compose(&c).await;
        }
    }

    async fn on_allow_key(&mut self, key: KeyEvent) {
        let mut reload = false;
        let mut close = false;
        if let Some(Modal::AllowList {
            entries,
            rejections,
            selected,
            input,
        }) = &mut self.modal
        {
            if let Some(text) = input {
                match key.code {
                    KeyCode::Esc => *input = None,
                    KeyCode::Enter => {
                        let node_id = text.trim().to_string();
                        let mut list =
                            AllowList::load_or_create(&self.paths.allowed_keys).unwrap();
                        match list.add(PeerEntry {
                            node_id,
                            name: None,
                            human: false,
                        }) {
                            Ok(()) => {
                                *input = None;
                                reload = true;
                            }
                            Err(e) => {
                                // surface error by keeping input; simple approach: clear + log via rejections pane is overkill
                                let _ = e;
                                *input = None;
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(ch) => text.push(ch),
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => close = true,
                    KeyCode::Char('a') => *input = Some(String::new()),
                    KeyCode::Char('j') | KeyCode::Down => {
                        if !entries.is_empty() {
                            *selected = (*selected + 1).min(entries.len() - 1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        if let Some(e) = entries.get(*selected) {
                            let query = e.node_id.clone();
                            if let Ok(mut list) =
                                AllowList::load_or_create(&self.paths.allowed_keys)
                            {
                                let _ = list.remove(&query);
                                reload = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            let _ = rejections;
        }
        if close {
            self.modal = None;
        }
        if reload {
            let entries = AllowList::load(&self.paths.allowed_keys)
                .map(|l| l.entries)
                .unwrap_or_default();
            let rejections = self.store.list_rejections().unwrap_or_default();
            if let Some(Modal::AllowList {
                entries: e2,
                rejections: r2,
                ..
            }) = &mut self.modal
            {
                *e2 = entries;
                *r2 = rejections;
            }
            let _ = self.refresh();
        }
    }
}

impl App {
    fn draw(&mut self, f: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(f.area());
        let main = Layout::horizontal([Constraint::Percentage(35), Constraint::Min(20)])
            .split(chunks[0]);

        self.draw_threads(f, main[0]);
        self.draw_messages(f, main[1]);
        self.draw_status(f, chunks[1]);

        match &self.modal {
            Some(Modal::Compose(_)) => self.draw_compose(f),
            Some(Modal::AllowList { .. }) => self.draw_allow(f),
            Some(Modal::Raw(m)) => self.draw_raw(f, m),
            Some(Modal::Help) => self.draw_help(f),
            None => {}
        }
    }

    fn draw_threads(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .threads
            .iter()
            .map(|t| {
                let peer = self.peer_display(&t.peer);
                let unread = if t.unread_count > 0 {
                    format!(" +{} ", t.unread_count)
                } else {
                    " ".to_string()
                };
                let preview: String = t.last_body.chars().take(30).collect();
                let when = t
                    .last_at
                    .map(|at| age(crate::util::now_secs().saturating_sub(at)))
                    .unwrap_or_default();
                let line = format!("{unread}{peer} — {preview} ({when})");
                let mut item = ListItem::new(line);
                if t.unread_count > 0 {
                    item = item.style(Style::default().add_modifier(Modifier::BOLD));
                }
                item
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" threads "),
            )
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol("▶ ");
        let mut state = ListState::default().with_selected(
            if self.threads.is_empty() {
                None
            } else {
                Some(self.selected_thread)
            },
        );
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_messages(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mine = m.direction == "out";
                let who = if mine { "me" } else { "peer" };
                let header = format!(
                    "{} {} · {}",
                    who,
                    if mine {
                        short(&m.to_id)
                    } else {
                        short(&m.from_id)
                    },
                    age(crate::util::now_secs().saturating_sub(m.created_at)),
                );
                let mut lines = vec![Line::from(Span::styled(
                    header,
                    Style::default().fg(if mine { Color::Green } else { Color::Cyan }),
                ))];
                for l in m.body.lines() {
                    lines.push(Line::from(format!("  {l}")));
                }
                lines.push(Line::from(""));
                let item = ListItem::new(lines);
                if i == self.selected_msg && self.focus == Focus::Messages {
                    item.style(Style::default().bg(Color::DarkGray))
                } else {
                    item
                }
            })
            .collect();
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" conversation "),
        );
        f.render_widget(list, area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let outbox = if self.outbox_failed > 0 {
            format!("outbox:{} ({} failed)", self.backlog, self.outbox_failed)
        } else {
            format!("outbox:{}", self.backlog)
        };
        let text = format!(
            "id {} │ {} │ {}{}\nTab pane · j/k move · Enter open · c compose · r reply · v raw · a allowlist · g refresh · ? help · q quit",
            short(&self.me),
            outbox,
            if self.last_status.is_empty() { "" } else { "│ " },
            self.last_status,
        );
        let p = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: true });
        f.render_widget(p, area);
    }

    fn centered(&self, f: &mut Frame, percent_x: u16, percent_y: u16) -> Rect {
        let popup = Layout::vertical([Constraint::Percentage(percent_y)])
            .flex(ratatui::layout::Flex::Center)
            .split(f.area());
        Layout::horizontal([Constraint::Percentage(percent_x)])
            .flex(ratatui::layout::Flex::Center)
            .split(popup[0])[0]
    }

    fn draw_compose(&self, f: &mut Frame) {
        if let Some(Modal::Compose(c)) = &self.modal {
            let area = self.centered(f, 80, 70);
            f.render_widget(Clear, area);
            let title = match &c.reply_to {
                Some(_) => " reply (Ctrl-S send · Tab field · Esc cancel) ",
                None => " compose (Ctrl-S send · Tab field · Esc cancel) ",
            };
            let peer_style = if c.field == ComposeField::Peer {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            let body_style = if c.field == ComposeField::Body {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            let text = vec![
                Line::from(vec![
                    Span::styled("to: ", Style::default().fg(Color::Yellow)),
                    Span::styled(&c.peer, peer_style),
                ]),
                Line::from(""),
                Span::styled(c.body.as_str(), body_style).into(),
                Line::from(""),
                Line::from(Span::styled(
                    format!("status: {}", c.status),
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            let p = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(title))
                .wrap(Wrap { trim: false });
            f.render_widget(p, area);
        }
    }

    fn draw_allow(&self, f: &mut Frame) {
        if let Some(Modal::AllowList {
            entries,
            rejections,
            selected,
            input,
        }) = &self.modal
        {
            let area = self.centered(f, 90, 85);
            f.render_widget(Clear, area);
            let mut lines = vec![Line::from(Span::styled(
                "allowed peers (a add · d remove · j/k select · Esc close)",
                Style::default().fg(Color::Yellow),
            ))];
            for (i, e) in entries.iter().enumerate() {
                let kind = if e.human { "human" } else { "agent" };
                let name = e.name.as_deref().unwrap_or("");
                let style = if i == *selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(
                    format!("  {name}  {kind}  {}", e.node_id),
                    style,
                )));
            }
            if let Some(text) = input {
                lines.push(Line::from(Span::styled(
                    format!("new node id: {text}"),
                    Style::default().bg(Color::DarkGray),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("rejected inbound attempts ({} unique)", rejections.len()),
                Style::default().fg(Color::Red),
            )));
            for r in rejections.iter().take(10) {
                lines.push(Line::from(format!(
                    "  {} ×{} (last {})",
                    r.node_id,
                    r.count,
                    age(crate::util::now_secs().saturating_sub(r.last_seen)),
                )));
            }
            let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" allowlist "));
            f.render_widget(p, area);
        }
    }

    fn draw_raw(&self, f: &mut Frame, m: &StoredMessage) {
        let area = self.centered(f, 80, 80);
        f.render_widget(Clear, area);
        let json = serde_json::to_string_pretty(m).unwrap_or_default();
        let p = Paragraph::new(json)
            .block(Block::default().borders(Borders::ALL).title(" raw message (Esc close) "))
            .wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }

    fn draw_help(&self, f: &mut Frame) {
        let area = self.centered(f, 70, 70);
        f.render_widget(Clear, area);
        let text = "\
agent-mail TUI keys

  Tab        switch threads / conversation pane
  j/k, ↑/↓   move selection
  Enter      open selected thread (marks read)
  c          compose to selected thread's peer
  r          reply to latest message in thread
  v          raw JSON view of selected message
  a          allowlist editor (add/remove, rejected attempts)
  g          refresh
  q          quit

Compose: Tab switch field · Ctrl-S send · Esc cancel
";
        let p = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" help (Esc close) "));
        f.render_widget(p, area);
    }
}
