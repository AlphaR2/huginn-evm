//! The Huginn TUI. Two views of the same run:
//!   - guided (default): your message, four stages (send, finalize, prove, verify), and
//!     plain sentences of what is happening and what happened;
//!   - details (`d`): every step with the exact command it ran and its output.

use {
    crate::steps::{Event, ProofMode, Runner, Settings, Side, State, StepId},
    anyhow::Result,
    ratatui::{
        crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers},
        layout::{Alignment, Constraint, Layout, Rect},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap},
        Frame,
    },
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
    tokio::{
        runtime::Runtime,
        sync::{mpsc, Mutex},
    },
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Rgb(153, 102, 255); // Solana-ish purple
const EVM: Color = Color::Rgb(0, 82, 255); // Base blue
const OK: Color = Color::Rgb(80, 200, 120);
const BAD: Color = Color::Rgb(235, 87, 87);
const DIM: Color = Color::DarkGray;

#[derive(Clone)]
enum Status {
    Pending,
    Running(Instant),
    Ok(String, Duration),
    Failed(String, Duration),
}

struct StepView {
    id: StepId,
    status: Status,
    log: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Guided,
    Details,
}

/// The four stages the guided view shows, and the steps each one covers.
const STAGES: [(&str, &[StepId]); 4] = [
    ("Send", &[StepId::Doctor, StepId::Send]),
    ("Finalize", &[StepId::Finalize, StepId::Fetch]),
    ("Prove", &[StepId::Prove]),
    ("Verify", &[StepId::Anvil, StepId::Deploy, StepId::Submit, StepId::Check, StepId::Greeter, StepId::Command, StepId::Forged]),
];

struct App {
    view: View,
    help: bool,
    run_started: Option<Instant>,
    run_took: Option<Duration>,
    steps: Vec<StepView>,
    list: ListState,
    state: State,
    settings: Settings,
    busy: bool,
    editing: Option<String>,
    scroll: u16,
    follow: bool,
    tick: usize,
    quit: bool,
}

pub fn run(settings: Settings) -> Result<()> {
    let rt = Runtime::new()?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = Runner::new(settings.clone(), tx);
    // Show what an earlier run found (proofs/state.json).
    crate::steps::send_state(&runner);
    let runner = Arc::new(Mutex::new(runner));
    let mut app = App::new(settings);

    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        while !app.quit {
            while let Ok(ev) = rx.try_recv() {
                app.apply(ev);
            }
            terminal.draw(|f| app.draw(f))?;
            if event::poll(Duration::from_millis(80))? {
                if let TermEvent::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        app.key(key.code, key.modifiers, &rt, &runner);
                    }
                }
            }
            app.tick = app.tick.wrapping_add(1);
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

impl App {
    fn new(settings: Settings) -> App {
        App {
            view: View::Guided,
            help: false,
            run_started: None,
            run_took: None,
            steps: StepId::ALL.into_iter().map(|id| StepView { id, status: Status::Pending, log: Vec::new() }).collect(),
            list: ListState::default().with_selected(Some(0)),
            state: State::default(),
            settings,
            busy: false,
            editing: None,
            scroll: 0,
            follow: true,
            tick: 0,
            quit: false,
        }
    }

    fn selected(&self) -> usize {
        self.list.selected().unwrap_or(0)
    }

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::Started(id) => {
                let i = index(id);
                self.steps[i].status = Status::Running(Instant::now());
                self.steps[i].log.clear();
                // Follow the running step, so a full run plays like a story.
                self.list.select(Some(i));
                self.scroll = 0;
                self.follow = true;
            }
            Event::Log(id, line) => self.steps[index(id)].log.push(line),
            Event::Done(id, result, took) => {
                self.steps[index(id)].status = match result {
                    Ok(summary) => Status::Ok(summary, took),
                    Err(e) => Status::Failed(e, took),
                };
            }
            Event::State(s) => self.state = *s,
            Event::Finished => {
                self.busy = false;
                self.run_took = self.run_started.map(|t| t.elapsed());
            }
        }
    }

    /// Runs `ids` in order on the runtime, stopping at the first failure.
    fn start(&mut self, ids: Vec<StepId>, rt: &Runtime, runner: &Arc<Mutex<Runner>>) {
        if self.busy || ids.is_empty() {
            return;
        }
        self.busy = true;
        self.run_started = Some(Instant::now());
        self.run_took = None;
        for id in &ids {
            self.steps[index(*id)].status = Status::Pending;
        }
        let runner = runner.clone();
        let settings = self.settings.clone();
        rt.spawn(async move {
            let mut r = runner.lock().await;
            r.settings.memo = settings.memo;
            r.settings.mode = settings.mode;
            r.settings.certs = settings.certs;
            for id in ids {
                if !r.run(id).await {
                    break;
                }
            }
            r.finished();
        });
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers, rt: &Runtime, runner: &Arc<Mutex<Runner>>) {
        if let Some(text) = &mut self.editing {
            match code {
                KeyCode::Enter => {
                    if !text.trim().is_empty() {
                        self.settings.memo = text.trim().to_string();
                    }
                    self.editing = None;
                }
                KeyCode::Esc => self.editing = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if text.len() < 120 => text.push(c),
                _ => {}
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if self.view == View::Details => self.view = View::Guided,
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('d') | KeyCode::Tab => {
                self.view = if self.view == View::Guided { View::Details } else { View::Guided };
            }
            KeyCode::Char('?') => self.help = true,
            // The guided view only starts, edits and changes settings.
            _ if self.view == View::Guided => match code {
                KeyCode::Enter if !self.busy => {
                    for s in &mut self.steps {
                        s.status = Status::Pending;
                        s.log.clear();
                    }
                    self.start(StepId::ALL.to_vec(), rt, runner);
                }
                KeyCode::Char('e') if !self.busy => self.editing = Some(String::new()),
                KeyCode::Char('m') if !self.busy => self.settings.mode = self.settings.mode.next(),
                KeyCode::Char('s') if !self.busy => self.settings.certs = self.settings.certs.next(),
                _ => {}
            },
            KeyCode::Down | KeyCode::Char('j') => {
                self.list.select(Some((self.selected() + 1).min(self.steps.len() - 1)));
                self.scroll = 0;
                self.follow = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.list.select(Some(self.selected().saturating_sub(1)));
                self.scroll = 0;
                self.follow = true;
            }
            KeyCode::PageDown | KeyCode::Char('J') => {
                self.follow = false;
                self.scroll = self.scroll.saturating_add(5);
            }
            KeyCode::PageUp | KeyCode::Char('K') => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(5);
            }
            KeyCode::Char('f') => self.follow = true,
            KeyCode::Enter | KeyCode::Char('a') if !self.busy => {
                for s in &mut self.steps {
                    s.status = Status::Pending;
                    s.log.clear();
                }
                self.start(StepId::ALL.to_vec(), rt, runner);
            }
            KeyCode::Char('r') if !self.busy => {
                let id = self.steps[self.selected()].id;
                self.start(vec![id], rt, runner);
            }
            KeyCode::Char('n') if !self.busy => {
                // Run from the selected step to the end.
                let from = self.selected();
                self.start(StepId::ALL[from..].to_vec(), rt, runner);
            }
            KeyCode::Char('m') if !self.busy => self.settings.mode = self.settings.mode.next(),
            KeyCode::Char('s') if !self.busy => self.settings.certs = self.settings.certs.next(),
            KeyCode::Char('e') if !self.busy => self.editing = Some(self.settings.memo.clone()),
            _ => {}
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        match self.view {
            View::Guided => self.draw_guided(f),
            View::Details => self.draw_details(f),
        }
        if self.help {
            self.draw_help(f);
        }
    }

    fn draw_details(&mut self, f: &mut Frame) {
        let [header, flow, body, settings, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(6),
            Constraint::Min(10),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());

        self.draw_header(f, header);
        self.draw_flow(f, flow);
        let [left, right] = Layout::horizontal([Constraint::Length(52), Constraint::Min(30)]).areas(body);
        self.draw_steps(f, left);
        self.draw_detail(f, right);
        self.draw_settings(f, settings);
        self.draw_keys(f, keys);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled(" HUGINN ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw("  Solana transaction  "),
            Span::styled("──▶", Style::new().fg(DIM)),
            Span::raw("  SP1 proof  "),
            Span::styled("──▶", Style::new().fg(DIM)),
            Span::raw("  EVM contract"),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_flow(&self, f: &mut Frame, area: Rect) {
        let [a, b, c] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(area);
        let s = &self.state;
        let short = |v: &Option<String>| v.as_deref().map(shorten).unwrap_or_else(|| "—".into());

        let solana = vec![
            kv("tx", short(&s.signature)),
            kv("slot", s.slot.map(|x| x.to_string()).unwrap_or("—".into())),
            kv(
                "status",
                match (s.slot, s.tx_ok) {
                    (Some(_), Some(true)) => "finalized ✓".into(),
                    (Some(_), Some(false)) => "finalized, tx failed".into(),
                    _ => "—".into(),
                },
            ),
            kv("memo", s.signature.as_ref().map(|_| format!("\"{}\"", self.settings.memo)).unwrap_or("—".into())),
        ];
        let proof = vec![
            kv("mode", s.proof_mode.map(|m| m.as_str().to_string()).unwrap_or("—".into())),
            kv("cycles", s.cycles.map(fmt_cycles).unwrap_or("—".into())),
            kv("time", s.prove_secs.map(|t| format!("{t:.1}s")).unwrap_or("—".into())),
            kv("key", short(&s.program_vkey)),
        ];
        let evm = vec![
            kv("client", short(&s.client)),
            kv("gas", s.submit_gas.map(|g| g.to_string()).unwrap_or("—".into())),
            kv(
                "verified",
                match (s.verified, s.forged_rejected) {
                    (Some(true), Some(true)) => "yes ✓ · forgery rejected".into(),
                    (Some(true), _) => "yes ✓".into(),
                    (Some(false), _) => "no".into(),
                    _ => "—".into(),
                },
            ),
            kv("greeting", s.greeting.as_ref().map(|g| format!("\"{g}\"")).unwrap_or("—".into())),
        ];
        let chain = match s.chain_id {
            Some(31337) => " EVM · anvil ".to_string(),
            Some(84532) => " EVM · Base Sepolia ".to_string(),
            Some(8453) => " EVM · Base ".to_string(),
            Some(id) => format!(" EVM · chain {id} "),
            None => " EVM ".to_string(),
        };
        for (area, title, color, lines) in
            [(a, " SOLANA ".to_string(), ACCENT, solana), (b, " PROOF ".to_string(), Color::Yellow, proof), (c, chain, EVM, evm)]
        {
            let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(color)).title(Span::styled(title, Style::new().fg(color).bold()));
            f.render_widget(Paragraph::new(lines).block(block), area);
        }
    }

    fn draw_steps(&mut self, f: &mut Frame, area: Rect) {
        let mut items = Vec::new();
        let mut last_side = None;
        for (i, s) in self.steps.iter().enumerate() {
            let side = s.id.side();
            let section = if last_side != Some(side) {
                last_side = Some(side);
                Some(match side {
                    Side::Setup => "",
                    Side::Solana => "solana",
                    Side::Proof => "proof",
                    Side::Evm => "evm",
                })
            } else {
                None
            };
            let (icon, icon_style, time) = match &s.status {
                Status::Pending => ("·".to_string(), Style::new().fg(DIM), String::new()),
                Status::Running(t) => (SPINNER[self.tick % SPINNER.len()].to_string(), Style::new().fg(Color::Yellow), format!("{:.0}s", t.elapsed().as_secs_f64())),
                Status::Ok(_, d) => ("✓".to_string(), Style::new().fg(OK).bold(), fmt_dur(*d)),
                Status::Failed(_, d) => ("✗".to_string(), Style::new().fg(BAD).bold(), fmt_dur(*d)),
            };
            let tag = match section {
                Some(t) if !t.is_empty() => Span::styled(format!("{t:<7}"), Style::new().fg(side_color(side)).add_modifier(Modifier::ITALIC)),
                _ => Span::raw("       "),
            };
            let width = area.width.saturating_sub(4) as usize;
            let name = format!("{:>2}  {}", i + 1, s.id.title());
            let pad = width.saturating_sub(7 + 2 + name.chars().count() + time.len());
            items.push(ListItem::new(Line::from(vec![
                tag,
                Span::styled(format!("{icon} "), icon_style),
                Span::raw(name),
                Span::raw(" ".repeat(pad)),
                Span::styled(time, Style::new().fg(DIM)),
            ])));
        }
        let title = if self.busy { " steps · running " } else { " steps " };
        let list = List::new(items)
            .block(Block::bordered().border_type(BorderType::Rounded).title(title))
            .highlight_style(Style::new().bg(Color::Rgb(40, 40, 55)).add_modifier(Modifier::BOLD));
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect) {
        let s = &self.steps[self.selected()];
        let mut lines = vec![Line::from(Span::styled(s.id.about(), Style::new().fg(Color::Gray))), Line::raw("")];
        match &s.status {
            Status::Ok(summary, _) => lines.push(Line::from(vec![Span::styled("✓ ", Style::new().fg(OK).bold()), Span::styled(summary.clone(), Style::new().fg(OK))])),
            Status::Failed(e, _) => lines.push(Line::from(vec![Span::styled("✗ ", Style::new().fg(BAD).bold()), Span::styled(e.clone(), Style::new().fg(BAD))])),
            Status::Running(_) => lines.push(Line::styled("running…", Style::new().fg(Color::Yellow))),
            Status::Pending => lines.push(Line::styled("not run yet: [r] runs this step, [n] runs from here to the end", Style::new().fg(DIM))),
        }
        lines.push(Line::raw(""));
        for l in &s.log {
            let style = if l.starts_with('$') {
                Style::new().fg(Color::Cyan)
            } else if l.starts_with('✓') {
                Style::new().fg(OK)
            } else if l.starts_with('✗') {
                Style::new().fg(BAD)
            } else if l.starts_with('!') {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            lines.push(Line::styled(l.clone(), style));
        }
        let inner_h = area.height.saturating_sub(2);
        let block = Block::bordered().border_type(BorderType::Rounded).title(format!(" {}. {} ", self.selected() + 1, s.id.title()));
        let width = area.width.saturating_sub(2).max(1) as usize;
        let total: u16 = lines.iter().map(|l| l.width().max(1).div_ceil(width) as u16).sum();
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let max = total.saturating_sub(inner_h);
        if self.follow {
            self.scroll = max;
        } else {
            self.scroll = self.scroll.min(max);
        }
        f.render_widget(para.block(block).scroll((self.scroll, 0)), area);
    }

    fn draw_settings(&self, f: &mut Frame, area: Rect) {
        let memo = match &self.editing {
            Some(t) => Span::styled(format!("{t}▏"), Style::new().fg(Color::Black).bg(Color::Yellow)),
            None => Span::styled(format!("\"{}\"", self.settings.memo), Style::new().fg(Color::White).bold()),
        };
        let mode_note = match self.settings.mode {
            ProofMode::Mock => "seconds, no real proof",
            ProofMode::Compressed => "real STARK, ~5 min",
            ProofMode::Groth16 => "real Groth16, Docker",
        };
        let line = Line::from(vec![
            Span::styled(" memo ", Style::new().fg(DIM)),
            memo,
            Span::styled("   proof ", Style::new().fg(DIM)),
            Span::styled(self.settings.mode.as_str(), Style::new().fg(Color::Yellow).bold()),
            Span::styled(format!(" ({mode_note})"), Style::new().fg(DIM)),
            Span::styled("   source ", Style::new().fg(DIM)),
            Span::raw(format!("ledger, certs from {}", self.settings.certs.as_str())),
            Span::styled("   evm ", Style::new().fg(DIM)),
            Span::raw(self.settings.evm_rpc()),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_keys(&self, f: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = if self.editing.is_some() {
            &[("enter", "save memo"), ("esc", "cancel")]
        } else {
            &[("d", "back to guided"), ("enter", "run all"), ("r", "run step"), ("n", "run from here"), ("↑↓", "select"), ("J/K", "scroll log"), ("q", "quit")]
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, v) in keys {
            spans.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(Color::Gray)));
            spans.push(Span::styled(format!(" {v}  "), Style::new().fg(DIM)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).alignment(Alignment::Left), area);
    }
}

// ---- guided view ----------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum StageStatus {
    Pending,
    Running,
    Ok,
    Failed,
}

impl App {
    fn step(&self, id: StepId) -> &StepView {
        &self.steps[index(id)]
    }

    fn stage_status(&self, steps: &[StepId]) -> StageStatus {
        let st: Vec<&Status> = steps.iter().map(|id| &self.step(*id).status).collect();
        if st.iter().any(|s| matches!(s, Status::Failed(..))) {
            StageStatus::Failed
        } else if st.iter().all(|s| matches!(s, Status::Ok(..))) {
            StageStatus::Ok
        } else if st.iter().any(|s| matches!(s, Status::Running(_))) || (self.busy && st.iter().any(|s| matches!(s, Status::Ok(..)))) {
            StageStatus::Running
        } else {
            StageStatus::Pending
        }
    }

    fn stage_note(&self, stage: usize) -> String {
        let s = &self.state;
        match (stage, self.stage_status(STAGES[stage].1)) {
            (_, StageStatus::Failed) => "stopped".into(),
            (_, StageStatus::Pending) => String::new(),
            (_, StageStatus::Running) => "…".into(),
            (0, StageStatus::Ok) => "sent".into(),
            (1, StageStatus::Ok) => s.slot.map(|x| format!("slot {x}")).unwrap_or_default(),
            (2, StageStatus::Ok) => match (s.proof_mode, s.prove_secs) {
                (Some(m), Some(t)) => format!("{}, {}", mode_name(m), fmt_dur(Duration::from_secs_f64(t))),
                _ => "done".into(),
            },
            (3, StageStatus::Ok) => "trusted".into(),
            _ => String::new(),
        }
    }

    fn draw_guided(&self, f: &mut Frame) {
        let full = f.area();
        // A calm, centered column.
        let width = full.width.min(92);
        let area = Rect { x: full.x + (full.width - width) / 2, width, ..full };
        let [_, title, _, message, _, stages, notes, _, card, footer, _] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(17),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(area);

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" HUGINN ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
                Span::styled("  prove a Solana message on an EVM chain", Style::new().fg(Color::Gray)),
            ])),
            title,
        );

        let msg = match &self.editing {
            Some(t) => vec![
                Span::styled("  Your message   ", Style::new().fg(DIM)),
                Span::styled(format!("{t}▏"), Style::new().fg(Color::White).bold()),
                Span::styled("   enter to save, esc to cancel", Style::new().fg(DIM)),
            ],
            None => vec![Span::styled("  Your message   ", Style::new().fg(DIM)), Span::styled(self.settings.memo.clone(), Style::new().fg(Color::White).bold())],
        };
        f.render_widget(Paragraph::new(Line::from(msg)), message);

        // ✓ Send ────── ✓ Finalize ────── ⠋ Prove ────── ○ Verify
        let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(stages);
        let note_cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(notes);
        for (i, (name, steps)) in STAGES.iter().enumerate() {
            let (icon, style) = match self.stage_status(steps) {
                StageStatus::Pending => ("○".to_string(), Style::new().fg(DIM)),
                StageStatus::Running => (SPINNER[self.tick % SPINNER.len()].to_string(), Style::new().fg(Color::Yellow).bold()),
                StageStatus::Ok => ("✓".to_string(), Style::new().fg(OK).bold()),
                StageStatus::Failed => ("✗".to_string(), Style::new().fg(BAD).bold()),
            };
            let line = Line::from(vec![Span::raw("  "), Span::styled(format!("{icon} {name}"), style), Span::styled(if i < 3 { " ─────" } else { "" }, Style::new().fg(DIM))]);
            f.render_widget(Paragraph::new(line), cols[i]);
            f.render_widget(Paragraph::new(Line::styled(format!("    {}", self.stage_note(i)), Style::new().fg(DIM))), note_cols[i]);
        }

        self.draw_card(f, card);

        let keys: &[(&str, &str)] = if self.editing.is_some() {
            &[]
        } else if self.busy {
            &[("d", "see the details"), ("q", "quit")]
        } else if self.steps.iter().any(|s| !matches!(s.status, Status::Pending)) {
            &[("enter", "run again"), ("e", "new message"), ("d", "details"), ("?", "help"), ("q", "quit")]
        } else {
            &[("enter", "start"), ("e", "edit message"), ("d", "details"), ("?", "help"), ("q", "quit")]
        };
        let mut spans = vec![Span::raw("  ")];
        for (k, v) in keys {
            spans.push(Span::styled(*k, Style::new().fg(Color::White).bold()));
            spans.push(Span::styled(format!(" {v}    "), Style::new().fg(DIM)));
        }
        let mode = Line::from(vec![
            Span::styled(format!("  {} proof", mode_name(self.settings.mode)), Style::new().fg(DIM)),
            Span::styled("  (m to change)", Style::new().fg(Color::Rgb(70, 70, 70))),
        ]);
        f.render_widget(Paragraph::new(vec![Line::from(spans), mode]), footer);
    }

    fn draw_card(&self, f: &mut Frame, area: Rect) {
        let failed = self.steps.iter().find(|s| matches!(s.status, Status::Failed(..)));
        let any_run = self.steps.iter().any(|s| !matches!(s.status, Status::Pending));
        let all_ok = self.steps.iter().all(|s| matches!(s.status, Status::Ok(..)));
        let mut lines: Vec<Line> = Vec::new();
        let border;

        if !any_run {
            border = DIM;
            lines.push(Line::raw(""));
            lines.push(Line::raw("  Huginn shows another blockchain that something really happened on Solana,"));
            lines.push(Line::raw("  using only the signatures of Solana's own validators."));
            lines.push(Line::raw(""));
            lines.push(Line::styled("  Press enter and your message will:", Style::new().fg(Color::Gray)));
            for (n, t) in [
                "be sent on Solana",
                "be finalized by Solana's validators",
                "become a zero-knowledge proof",
                "be checked by a contract on an EVM chain",
                "change a contract that only your Solana wallet controls",
            ]
            .iter()
            .enumerate()
            {
                lines.push(Line::from(vec![Span::styled(format!("    {}  ", n + 1), Style::new().fg(ACCENT)), Span::raw(*t)]));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(format!("  {}", time_hint(self.settings.mode)), Style::new().fg(DIM)));
        } else {
            // The story so far, in plain sentences.
            for s in &self.steps {
                match &s.status {
                    Status::Ok(..) => lines.push(Line::from(vec![Span::styled("  ✓ ", Style::new().fg(OK)), Span::raw(done_text(s.id, &self.state, &self.settings))])),
                    Status::Running(t) => lines.push(Line::from(vec![
                        Span::styled(format!("  {} ", SPINNER[self.tick % SPINNER.len()]), Style::new().fg(Color::Yellow)),
                        Span::styled(doing_text(s.id), Style::new().fg(Color::White).bold()),
                        Span::styled(format!("  {}", fmt_dur(t.elapsed())), Style::new().fg(DIM)),
                    ])),
                    Status::Failed(..) => lines.push(Line::from(vec![Span::styled("  ✗ ", Style::new().fg(BAD)), Span::styled(doing_text(s.id).trim_end_matches('…').to_string(), Style::new().fg(BAD))])),
                    Status::Pending => {}
                }
                if s.id == StepId::Prove && matches!(s.status, Status::Running(_)) {
                    lines.push(Line::styled(format!("    {}", prove_hint(self.settings.mode)), Style::new().fg(DIM)));
                }
            }
            if let Some(s) = failed {
                border = BAD;
                lines.push(Line::raw(""));
                lines.extend(failure_help(s).into_iter().map(|l| Line::styled(format!("  {l}"), Style::new().fg(Color::Gray))));
            } else if all_ok && !self.busy {
                border = OK;
                lines.push(Line::raw(""));
                let took = self.run_took.map(|d| format!(" in {}", fmt_dur(d))).unwrap_or_default();
                lines.push(Line::styled(format!("  Done{took}. A contract on {} trusts your Solana message.", chain_name(self.state.chain_id)), Style::new().fg(OK).bold()));
                if self.settings.mode == ProofMode::Mock || self.state.proof_mode == Some(ProofMode::Mock) {
                    lines.push(Line::styled("  Practice proof: every check ran, but no real proof was made. Press m for a real one.", Style::new().fg(DIM)));
                }
            } else {
                border = Color::Yellow;
            }
        }
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(border));
        f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
    }

    fn draw_help(&self, f: &mut Frame) {
        let full = f.area();
        let (w, h) = (86.min(full.width), 17.min(full.height));
        let area = Rect { x: full.x + (full.width - w) / 2, y: full.y + (full.height - h) / 2, width: w, height: h };
        let k = |key: &str, text: &str| Line::from(vec![Span::styled(format!("  {key:<7}"), Style::new().fg(Color::White).bold()), Span::styled(text.to_string(), Style::new().fg(Color::Gray))]);
        let lines = vec![
            Line::raw(""),
            k("enter", "send your message and run everything"),
            k("e", "write a new message"),
            k("d", "details: every step, the exact command it ran, and its output"),
            k("q", "quit"),
            Line::raw(""),
            Line::styled("  Settings", Style::new().fg(ACCENT).bold()),
            k("m", &format!("proof: {}", mode_name(self.settings.mode))),
            Line::styled("         practice: seconds; SP1 runs every check but makes no real proof", Style::new().fg(DIM)),
            Line::styled("         real (STARK): ~5 minutes on a laptop", Style::new().fg(DIM)),
            Line::styled("         real (Groth16): what real chains take; ~10+ min, needs Docker", Style::new().fg(DIM)),
            k("s", &format!("validators' signatures from: {}", match self.settings.certs {
                crate::steps::CertSource::Shreds => "the next block (no plugin needed)",
                crate::steps::CertSource::Plugin => "the footer-dump plugin",
            })),
            Line::raw(""),
            Line::styled("  any key closes this", Style::new().fg(DIM)),
        ];
        f.render_widget(ratatui::widgets::Clear, area);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT)).title(" help ");
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

fn mode_name(m: ProofMode) -> &'static str {
    match m {
        ProofMode::Mock => "practice",
        ProofMode::Compressed => "real (STARK)",
        ProofMode::Groth16 => "real (Groth16)",
    }
}

fn time_hint(m: ProofMode) -> &'static str {
    match m {
        ProofMode::Mock => "Takes about 10 seconds with a practice proof (press m for a real one).",
        ProofMode::Compressed => "Takes about 5 minutes: the real proof is the slow part.",
        ProofMode::Groth16 => "Takes 10+ minutes: the real Groth16 proof is the slow part.",
    }
}

fn prove_hint(m: ProofMode) -> &'static str {
    match m {
        ProofMode::Mock => "practice proof: a few seconds",
        ProofMode::Compressed => "a real proof: usually about 5 minutes. Your laptop's fans may spin up.",
        ProofMode::Groth16 => "a real Groth16 proof: usually 10+ minutes, part of it in Docker.",
    }
}

fn chain_name(id: Option<u64>) -> String {
    match id {
        Some(31337) | None => "your local EVM chain".into(),
        Some(84532) => "Base Sepolia".into(),
        Some(8453) => "Base".into(),
        Some(id) => format!("chain {id}"),
    }
}

/// What a step is doing, in plain words.
fn doing_text(id: StepId) -> &'static str {
    match id {
        StepId::Doctor => "Checking your setup…",
        StepId::Send => "Sending your message on Solana…",
        StepId::Finalize => "Waiting for Solana's validators to finalize it…",
        StepId::Fetch => "Collecting the block and the validators' signatures…",
        StepId::Prove => "Making a zero-knowledge proof that your message is in a finalized block…",
        StepId::Anvil => "Starting a local EVM chain…",
        StepId::Deploy => "Deploying the Huginn contract…",
        StepId::Submit => "Sending the proof to the contract…",
        StepId::Check => "Asking the contract whether it trusts your message…",
        StepId::Greeter => "Deploying a contract that only your Solana wallet controls…",
        StepId::Command => "Using your message to change that contract…",
        StepId::Forged => "Trying a forged copy of your message…",
    }
}

/// What a step did, in plain words.
fn done_text(id: StepId, s: &State, settings: &Settings) -> String {
    match id {
        StepId::Doctor => "Your setup is ready".into(),
        StepId::Send => "Sent your message on Solana".into(),
        StepId::Finalize => match (s.slot, s.tx_ok) {
            (Some(slot), Some(false)) => format!("Finalized in slot {slot} (the transaction itself failed, but it is in the block)"),
            (Some(slot), _) => format!("Solana's validators finalized it in slot {slot}"),
            _ => "Solana's validators finalized it".into(),
        },
        StepId::Fetch => "Collected the block and the validators' signatures, and checked them".into(),
        StepId::Prove => match s.proof_mode {
            Some(ProofMode::Mock) => "SP1 checked the signatures and found your message (practice proof)".into(),
            _ => format!("Made a real zero-knowledge proof{}", s.cycles.map(|c| format!(" ({} computation steps)", fmt_cycles(c))).unwrap_or_default()),
        },
        StepId::Anvil => format!("Using {}", chain_name(s.chain_id)),
        StepId::Deploy => "Deployed the Huginn contract".into(),
        StepId::Submit => "The contract checked the proof and accepted it".into(),
        StepId::Check => "The contract now says your message is verified".into(),
        StepId::Greeter => "Deployed a contract only your Solana wallet controls".into(),
        StepId::Command => format!("Your message changed it: it now says \"{}\"", s.greeting.clone().unwrap_or_else(|| settings.memo.clone())),
        StepId::Forged => "A forged copy of your message was rejected".into(),
    }
}

/// What went wrong and what to do, in plain words.
fn failure_help(s: &StepView) -> Vec<String> {
    let Status::Failed(err, _) = &s.status else { return vec![] };
    let mut out = Vec::new();
    if s.id == StepId::Doctor {
        out.push("Something Huginn needs isn't ready:".to_string());
        let mut lines = s.log.iter().peekable();
        while let Some(l) = lines.next() {
            if let Some(rest) = l.strip_prefix("✗ ") {
                out.push(format!("  • {}", rest.split_whitespace().collect::<Vec<_>>().join(" ")));
                if let Some(next) = lines.peek() {
                    if next.starts_with("    ") {
                        out.push(format!("    fix: {}", next.trim()));
                    }
                }
            }
        }
    } else {
        out.push(format!("It stopped here: {}", err.lines().next().unwrap_or(err)));
    }
    out.push(String::new());
    out.push("Press d to see exactly what happened, then enter to try again.".into());
    out
}

fn index(id: StepId) -> usize {
    StepId::ALL.iter().position(|s| *s == id).unwrap()
}

fn side_color(side: Side) -> Color {
    match side {
        Side::Setup => DIM,
        Side::Solana => ACCENT,
        Side::Proof => Color::Yellow,
        Side::Evm => EVM,
    }
}

fn kv(k: &str, v: String) -> Line<'static> {
    Line::from(vec![Span::styled(format!(" {k:<9}"), Style::new().fg(DIM)), Span::raw(v)])
}

fn shorten(s: &str) -> String {
    let n = s.chars().count();
    if n <= 16 {
        s.to_string()
    } else {
        let a: String = s.chars().take(8).collect();
        let b: String = s.chars().skip(n - 6).collect();
        format!("{a}…{b}")
    }
}

fn fmt_cycles(c: u64) -> String {
    if c >= 1_000_000 {
        format!("{:.2}M", c as f64 / 1e6)
    } else {
        format!("{}k", c / 1000)
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{:.0}ms", s * 1000.0)
    } else if s < 120.0 {
        format!("{s:.1}s")
    } else {
        format!("{}m{:02}s", s as u64 / 60, s as u64 % 60)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, ratatui::{backend::TestBackend, Terminal}};

    /// Renders the screen after a real headless run (`proofs/state.json`) into text:
    /// `cargo test --release --offline -- --nocapture snapshot`.
    #[test]
    fn snapshot() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let settings = Settings::load(dir.canonicalize().unwrap(), None).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runner = Runner::new(settings.clone(), tx);
        crate::steps::send_state(&runner);
        let mut app = App::new(settings);
        while let Ok(ev) = rx.try_recv() {
            app.apply(ev);
        }
        let summaries = [
            "ready (groth16 needs: circuits)",
            "signature 638WLVFo…",
            "slot 124404 finalized",
            "block_id rebuilt and certified",
            "mock proof in 0.4s, 5122326 cycles",
            "anvil started, chain 31337",
            "SolanaLightClient at 0xe7f1…",
            "accepted, 105765 gas",
            "isVerified(signature) = true",
            "SolanaGreeter at 0x8A79…C318, greeting gm",
            "greeting is now \"hello from solana\"",
            "forged memo rejected, real one accepted",
        ];
        for (i, s) in summaries.iter().enumerate() {
            app.steps[i].status = Status::Ok(s.to_string(), Duration::from_millis(300 + 400 * i as u64));
        }
        app.steps[11].log = vec![
            "1) the real output with its proof:".into(),
            "$ cast call 0xe7f1… verify(bytes,bytes) 0x01b1f252… 0x1d44d290… --rpc-url http://127.0.0.1:8545".into(),
            "   accepted".into(),
            "2) the same proof, memo's last character changed 'e' -> 'd':".into(),
            "$ cast call 0xe7f1… verify(bytes,bytes) 0x01b1f252… 0x1d44d290… --rpc-url http://127.0.0.1:8545".into(),
            "   rejected: InvalidProof".into(),
        ];
        app.list.select(Some(11));
        let mut t = Terminal::new(TestBackend::new(150, 38)).unwrap();
        app.view = if std::env::var("VIEW").as_deref() == Ok("details") { View::Details } else { View::Guided };
        app.run_took = Some(Duration::from_millis(9800));
        t.draw(|f| app.draw(f)).unwrap();
        let buf = t.backend().buffer();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
    }
}

#[cfg(test)]
mod guided_tests {
    use {super::*, ratatui::{backend::TestBackend, Terminal}};

    fn render(app: &mut App, title: &str) {
        let mut t = Terminal::new(TestBackend::new(110, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        println!("===== {title} =====");
        let buf = t.backend().buffer();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
    }

    fn app() -> App {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut settings = Settings::load(dir.canonicalize().unwrap(), None).unwrap();
        settings.memo = "gm from solana".into();
        App::new(settings)
    }

    /// `cargo test --release --offline -- --nocapture guided`
    #[test]
    fn guided_moments() {
        let mut a = app();
        render(&mut a, "before start");

        let ms = |n| Duration::from_millis(n);
        for (i, _) in StepId::ALL.iter().enumerate().take(4) {
            a.steps[i].status = Status::Ok(String::new(), ms(500));
        }
        a.state.slot = Some(126768);
        a.state.tx_ok = Some(true);
        a.steps[4].status = Status::Running(Instant::now() - ms(2300));
        a.settings.mode = ProofMode::Compressed;
        a.busy = true;
        render(&mut a, "proving");

        for s in &mut a.steps {
            s.status = Status::Ok(String::new(), ms(500));
        }
        a.busy = false;
        a.settings.mode = ProofMode::Mock;
        a.run_took = Some(ms(9800));
        a.state.proof_mode = Some(ProofMode::Mock);
        a.state.prove_secs = Some(0.4);
        a.state.chain_id = Some(31337);
        a.state.greeting = Some("gm from solana".into());
        render(&mut a, "done");

        let mut b = app();
        b.steps[0].status = Status::Failed("missing: validator".into(), ms(300));
        b.steps[0].log = vec![
            "✓ solana   solana-cli 4.4.0".into(),
            "✗ validator not reachable at http://127.0.0.1:8899".into(),
            "    in huginn: solana-test-validator --alpenglow --ledger .local-ledger".into(),
        ];
        render(&mut b, "setup problem");

        b.help = true;
        render(&mut b, "help");
    }
}
