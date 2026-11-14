//! The Huginn TUI. Two views of the same run:
//!   - guided (default): your message, five stages, and a live feed of the DATA each step
//!     finds (full hashes, validator counts, signer bitmaps, cycles, contract reads), laid out
//!     so it can be copied and checked; the same data is written to proofs/receipt.md;
//!   - details (`d`): every step with the exact command it ran and its output.

use {
    crate::steps::{
        chain_label, Cluster, Event, Fact, ProofMode, Runner, Settings, Side, State, StepId,
    },
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

/// The five stages the guided view shows, and the steps each one covers. "Set up" deploys
/// the contracts first, because your message names the greeter's address.
const STAGES: [(&str, &[StepId]); 5] = [
    (
        "Set up",
        &[
            StepId::Doctor,
            StepId::Anvil,
            StepId::Deploy,
            StepId::Greeter,
        ],
    ),
    ("Send", &[StepId::Send]),
    ("Finalize", &[StepId::Finalize, StepId::Fetch]),
    ("Prove", &[StepId::Prove]),
    (
        "Verify",
        &[
            StepId::Submit,
            StepId::Check,
            StepId::Command,
            StepId::Forged,
        ],
    ),
];

struct App {
    view: View,
    help: bool,
    run_started: Option<Instant>,
    run_took: Option<Duration>,
    steps: Vec<StepView>,
    list: ListState,
    state: State,
    facts: Vec<Fact>,
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
            steps: StepId::ALL
                .into_iter()
                .map(|id| StepView {
                    id,
                    status: Status::Pending,
                    log: Vec::new(),
                })
                .collect(),
            list: ListState::default().with_selected(Some(0)),
            state: State::default(),
            facts: Vec::new(),
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
                self.facts.retain(|f| f.step != id.name());
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
            Event::State(s) => {
                self.facts = s.facts.clone();
                self.state = *s;
            }
            Event::Fact(f) => {
                self.facts
                    .retain(|x| !(x.step == f.step && x.label == f.label));
                self.facts.push(f);
            }
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

    fn key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        rt: &Runtime,
        runner: &Arc<Mutex<Runner>>,
    ) {
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
                self.view = if self.view == View::Guided {
                    View::Details
                } else {
                    View::Guided
                };
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
                KeyCode::Char('s') if !self.busy => {
                    self.settings.certs = self.settings.certs.next()
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown | KeyCode::Char('J') => {
                    self.follow = false;
                    self.scroll = self.scroll.saturating_add(4);
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp | KeyCode::Char('K') => {
                    self.follow = false;
                    self.scroll = self.scroll.saturating_sub(4);
                }
                KeyCode::Char('f') | KeyCode::End => self.follow = true,
                _ => {}
            },
            KeyCode::Down | KeyCode::Char('j') => {
                self.list
                    .select(Some((self.selected() + 1).min(self.steps.len() - 1)));
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
        let [left, right] =
            Layout::horizontal([Constraint::Length(52), Constraint::Min(30)]).areas(body);
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
            kv(
                "memo",
                s.signature
                    .as_ref()
                    .map(|_| format!("\"{}\"", self.settings.memo))
                    .unwrap_or("—".into()),
            ),
        ];
        let proof = vec![
            kv(
                "mode",
                s.proof_mode
                    .map(|m| m.as_str().to_string())
                    .unwrap_or("—".into()),
            ),
            kv("cycles", s.cycles.map(fmt_cycles).unwrap_or("—".into())),
            kv(
                "time",
                s.prove_secs
                    .map(|t| format!("{t:.1}s"))
                    .unwrap_or("—".into()),
            ),
            kv("key", short(&s.program_vkey)),
        ];
        let evm = vec![
            kv("client", short(&s.client)),
            kv(
                "gas",
                s.submit_gas.map(|g| g.to_string()).unwrap_or("—".into()),
            ),
            kv(
                "verified",
                match (s.verified, s.forged_rejected) {
                    (Some(true), Some(true)) => "yes ✓ · forgery rejected".into(),
                    (Some(true), _) => "yes ✓".into(),
                    (Some(false), _) => "no".into(),
                    _ => "—".into(),
                },
            ),
            kv(
                "greeting",
                s.greeting
                    .as_ref()
                    .map(|g| format!("\"{g}\""))
                    .unwrap_or("—".into()),
            ),
        ];
        let chain = match s.chain_id {
            Some(31337) => " EVM · anvil ".to_string(),
            Some(84532) => " EVM · Base Sepolia ".to_string(),
            Some(8453) => " EVM · Base ".to_string(),
            Some(id) => format!(" EVM · chain {id} "),
            None => " EVM ".to_string(),
        };
        for (area, title, color, lines) in [
            (a, " SOLANA ".to_string(), ACCENT, solana),
            (b, " PROOF ".to_string(), Color::Yellow, proof),
            (c, chain, EVM, evm),
        ] {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(color))
                .title(Span::styled(title, Style::new().fg(color).bold()));
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
                Status::Running(t) => (
                    SPINNER[self.tick % SPINNER.len()].to_string(),
                    Style::new().fg(Color::Yellow),
                    format!("{:.0}s", t.elapsed().as_secs_f64()),
                ),
                Status::Ok(_, d) => ("✓".to_string(), Style::new().fg(OK).bold(), fmt_dur(*d)),
                Status::Failed(_, d) => ("✗".to_string(), Style::new().fg(BAD).bold(), fmt_dur(*d)),
            };
            let tag = match section {
                Some(t) if !t.is_empty() => Span::styled(
                    format!("{t:<7}"),
                    Style::new()
                        .fg(side_color(side))
                        .add_modifier(Modifier::ITALIC),
                ),
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
        let title = if self.busy {
            " steps · running "
        } else {
            " steps "
        };
        let list = List::new(items)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(title),
            )
            .highlight_style(
                Style::new()
                    .bg(Color::Rgb(40, 40, 55))
                    .add_modifier(Modifier::BOLD),
            );
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect) {
        let s = &self.steps[self.selected()];
        let mut lines = vec![
            Line::from(Span::styled(s.id.about(), Style::new().fg(Color::Gray))),
            Line::raw(""),
        ];
        match &s.status {
            Status::Ok(summary, _) => lines.push(Line::from(vec![
                Span::styled("✓ ", Style::new().fg(OK).bold()),
                Span::styled(summary.clone(), Style::new().fg(OK)),
            ])),
            Status::Failed(e, _) => lines.push(Line::from(vec![
                Span::styled("✗ ", Style::new().fg(BAD).bold()),
                Span::styled(e.clone(), Style::new().fg(BAD)),
            ])),
            Status::Running(_) => {
                lines.push(Line::styled("running…", Style::new().fg(Color::Yellow)))
            }
            Status::Pending => lines.push(Line::styled(
                "not run yet: [r] runs this step, [n] runs from here to the end",
                Style::new().fg(DIM),
            )),
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
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(format!(" {}. {} ", self.selected() + 1, s.id.title()));
        let width = area.width.saturating_sub(2).max(1) as usize;
        let total: u16 = lines
            .iter()
            .map(|l| l.width().max(1).div_ceil(width) as u16)
            .sum();
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
            Some(t) => Span::styled(
                format!("{t}▏"),
                Style::new().fg(Color::Black).bg(Color::Yellow),
            ),
            None => Span::styled(
                format!("\"{}\"", self.settings.memo),
                Style::new().fg(Color::White).bold(),
            ),
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
            Span::styled(
                self.settings.mode.as_str(),
                Style::new().fg(Color::Yellow).bold(),
            ),
            Span::styled(format!(" ({mode_note})"), Style::new().fg(DIM)),
            Span::styled("   source ", Style::new().fg(DIM)),
            Span::raw(format!(
                "ledger, certs from {}",
                self.settings.certs.as_str()
            )),
            Span::styled("   evm ", Style::new().fg(DIM)),
            Span::raw(redact::redact_url(&self.settings.evm_rpc())),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_keys(&self, f: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = if self.editing.is_some() {
            &[("enter", "save memo"), ("esc", "cancel")]
        } else {
            &[
                ("d", "back to guided"),
                ("enter", "run all"),
                ("r", "run step"),
                ("n", "run from here"),
                ("↑↓", "select"),
                ("J/K", "scroll log"),
                ("q", "quit"),
            ]
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, v) in keys {
            spans.push(Span::styled(
                format!(" {k} "),
                Style::new().fg(Color::Black).bg(Color::Gray),
            ));
            spans.push(Span::styled(format!(" {v}  "), Style::new().fg(DIM)));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Left),
            area,
        );
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
        } else if st.iter().any(|s| matches!(s, Status::Running(_)))
            || (self.busy && st.iter().any(|s| matches!(s, Status::Ok(..))))
        {
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
            (0, StageStatus::Ok) => "ready".into(),
            (1, StageStatus::Ok) => "sent".into(),
            (2, StageStatus::Ok) => s.slot.map(|x| format!("slot {x}")).unwrap_or_default(),
            (3, StageStatus::Ok) => match (s.proof_mode, s.prove_secs) {
                (Some(m), Some(t)) => {
                    format!("{}, {}", mode_name(m), fmt_dur(Duration::from_secs_f64(t)))
                }
                _ => "done".into(),
            },
            (4, StageStatus::Ok) => match s.proof_mode {
                Some(ProofMode::Groth16) => "trusted".into(),
                _ => "flow works".into(),
            },
            _ => String::new(),
        }
    }

    fn draw_guided(&mut self, f: &mut Frame) {
        let full = f.area();
        // A centered column, wide enough for a 64-digit hash and its label.
        let width = full.width.min(118);
        let area = Rect {
            x: full.x + (full.width - width) / 2,
            width,
            ..full
        };
        let [_, title, route, message, _, stages, notes, _, card, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .areas(area);

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" HUGINN ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
                Span::styled(
                    "  prove a Solana message on an EVM chain",
                    Style::new().fg(Color::Gray),
                ),
            ])),
            title,
        );

        let chain = self
            .state
            .chain_id
            .map(|c| format!("{c} ({})", chain_label(c)))
            .unwrap_or_else(|| "EVM".into());
        let cluster = match self.settings.cluster {
            Cluster::Local => "Solana local",
            Cluster::Devnet => "Solana devnet",
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("  {cluster}"), Style::new().fg(ACCENT)),
                Span::styled("  ──▶  ", Style::new().fg(DIM)),
                Span::styled("SP1 proof", Style::new().fg(Color::Yellow)),
                Span::styled("  ──▶  ", Style::new().fg(DIM)),
                Span::styled(format!("EVM chain {chain}"), Style::new().fg(EVM)),
            ])),
            route,
        );

        let msg = match &self.editing {
            Some(t) => vec![
                Span::styled("  Your message   ", Style::new().fg(DIM)),
                Span::styled(format!("{t}▏"), Style::new().fg(Color::White).bold()),
                Span::styled("   enter to save, esc to cancel", Style::new().fg(DIM)),
            ],
            None => vec![
                Span::styled("  Your message   ", Style::new().fg(DIM)),
                Span::styled(
                    self.settings.memo.clone(),
                    Style::new().fg(Color::White).bold(),
                ),
            ],
        };
        f.render_widget(Paragraph::new(Line::from(msg)), message);

        // ✓ Set up ─── ✓ Send ─── ✓ Finalize ─── ⠋ Prove ─── ○ Verify
        let n = STAGES.len() as u32;
        let cols = Layout::horizontal([Constraint::Ratio(1, n); STAGES.len()]).split(stages);
        let note_cols = Layout::horizontal([Constraint::Ratio(1, n); STAGES.len()]).split(notes);
        for (i, (name, steps)) in STAGES.iter().enumerate() {
            let (icon, style) = match self.stage_status(steps) {
                StageStatus::Pending => ("○".to_string(), Style::new().fg(DIM)),
                StageStatus::Running => (
                    SPINNER[self.tick % SPINNER.len()].to_string(),
                    Style::new().fg(Color::Yellow).bold(),
                ),
                StageStatus::Ok => ("✓".to_string(), Style::new().fg(OK).bold()),
                StageStatus::Failed => ("✗".to_string(), Style::new().fg(BAD).bold()),
            };
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{icon} {name}"), style),
                Span::styled(
                    if i + 1 < STAGES.len() {
                        " ───"
                    } else {
                        ""
                    },
                    Style::new().fg(DIM),
                ),
            ]);
            f.render_widget(Paragraph::new(line), cols[i]);
            f.render_widget(
                Paragraph::new(Line::styled(
                    format!("    {}", self.stage_note(i)),
                    Style::new().fg(DIM),
                )),
                note_cols[i],
            );
        }

        self.draw_data(f, card);

        let keys: &[(&str, &str)] = if self.editing.is_some() {
            &[]
        } else if self.busy {
            &[
                ("j/k", "scroll"),
                ("f", "follow"),
                ("d", "details"),
                ("q", "quit"),
            ]
        } else if self
            .steps
            .iter()
            .any(|s| !matches!(s.status, Status::Pending))
        {
            &[
                ("enter", "run again"),
                ("j/k", "scroll"),
                ("e", "new message"),
                ("d", "details"),
                ("?", "help"),
                ("q", "quit"),
            ]
        } else {
            &[
                ("enter", "start"),
                ("e", "edit message"),
                ("d", "details"),
                ("?", "help"),
                ("q", "quit"),
            ]
        };
        let mut spans = vec![Span::raw("  ")];
        for (k, v) in keys {
            spans.push(Span::styled(*k, Style::new().fg(Color::White).bold()));
            spans.push(Span::styled(format!(" {v}    "), Style::new().fg(DIM)));
        }
        let mode = Line::from(vec![
            Span::styled(
                format!("  {} proof", mode_name(self.settings.mode)),
                Style::new().fg(DIM),
            ),
            Span::styled("  (m to change)", Style::new().fg(Color::Rgb(70, 70, 70))),
        ]);
        f.render_widget(Paragraph::new(vec![Line::from(spans), mode]), footer);
    }

    /// The data feed: for every step that has started, its header and every fact it found,
    /// values in full so they can be selected, copied and checked.
    fn draw_data(&mut self, f: &mut Frame, area: Rect) {
        let failed = self
            .steps
            .iter()
            .find(|s| matches!(s.status, Status::Failed(..)));
        let any_run = self
            .steps
            .iter()
            .any(|s| !matches!(s.status, Status::Pending));
        let all_ok = self
            .steps
            .iter()
            .all(|s| matches!(s.status, Status::Ok(..)));
        let mut lines: Vec<Line> = Vec::new();
        let border;

        if !any_run {
            border = DIM;
            lines.push(Line::raw(""));
            lines.push(Line::raw(
                "  Huginn shows another blockchain that something really happened on Solana,",
            ));
            lines.push(Line::raw(
                "  using only the signatures of Solana's own validators.",
            ));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "  Press enter. As each step runs, its data appears here:",
                Style::new().fg(Color::Gray),
            ));
            for t in [
                "the Solana transaction: signature, slot, fee, explorer link",
                "the block: shreds, FEC sets, block_id rebuilt and certified",
                "the certificate: how many validators signed, their stake, the signer bitmap",
                "the proof: validator set hash, cycles, program key, what the proof commits to",
                "the contract: addresses, transaction hashes, values read back from the chain",
            ] {
                lines.push(Line::from(vec![
                    Span::styled("    · ", Style::new().fg(ACCENT)),
                    Span::raw(t),
                ]));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("  {}", time_hint(self.settings.mode)),
                Style::new().fg(DIM),
            ));
            lines.push(Line::styled("  Everything shown is also saved to proofs/receipt.md, with commands to check it yourself.", Style::new().fg(DIM)));
        } else {
            let width = area.width.saturating_sub(6).max(20) as usize;
            // One label column for the whole feed, as wide as the longest label (capped).
            let label_w = self
                .facts
                .iter()
                .map(|f| f.label.chars().count())
                .max()
                .unwrap_or(12)
                .clamp(12, 38)
                + 8;
            for s in &self.steps {
                let n = index(s.id) + 1;
                let title = s.id.title();
                match &s.status {
                    Status::Pending => continue,
                    Status::Ok(_, d) => lines.push(Line::from(vec![
                        Span::styled("  ✓ ", Style::new().fg(OK).bold()),
                        Span::styled(format!("{n:>2}  {title}"), Style::new().fg(OK).bold()),
                        Span::styled(format!("   {}", fmt_dur(*d)), Style::new().fg(DIM)),
                    ])),
                    Status::Running(t) => lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} ", SPINNER[self.tick % SPINNER.len()]),
                            Style::new().fg(Color::Yellow),
                        ),
                        Span::styled(
                            format!("{n:>2}  {title}"),
                            Style::new().fg(Color::White).bold(),
                        ),
                        Span::styled(format!("   {}", fmt_dur(t.elapsed())), Style::new().fg(DIM)),
                    ])),
                    Status::Failed(_, d) => lines.push(Line::from(vec![
                        Span::styled("  ✗ ", Style::new().fg(BAD).bold()),
                        Span::styled(format!("{n:>2}  {title}"), Style::new().fg(BAD).bold()),
                        Span::styled(format!("   {}", fmt_dur(*d)), Style::new().fg(DIM)),
                    ])),
                }
                for fact in self.facts.iter().filter(|x| x.step == s.id.name()) {
                    lines.extend(fact_lines(fact, width, label_w));
                }
                if let Status::Running(_) = s.status {
                    lines.push(Line::styled(
                        format!(
                            "      … {}",
                            s.log
                                .last()
                                .map(|l| l.trim().to_string())
                                .unwrap_or_else(|| doing_text(s.id).to_string())
                        ),
                        Style::new().fg(DIM),
                    ));
                    if s.id == StepId::Prove {
                        lines.push(Line::styled(
                            format!("      {}", prove_hint(self.settings.mode)),
                            Style::new().fg(DIM),
                        ));
                    }
                }
                lines.push(Line::raw(""));
            }
            if let Some(s) = failed {
                border = BAD;
                lines.extend(
                    failure_help(s)
                        .into_iter()
                        .map(|l| Line::styled(format!("  {l}"), Style::new().fg(Color::Gray))),
                );
            } else if all_ok && !self.busy {
                let took = self
                    .run_took
                    .map(|d| format!(" in {}", fmt_dur(d)))
                    .unwrap_or_default();
                let chain = chain_name(self.state.chain_id);
                match self.state.proof_mode.unwrap_or(self.settings.mode) {
                    ProofMode::Groth16 => {
                        border = OK;
                        lines.push(Line::styled(format!("  Done{took}. A real Groth16 proof was checked by the verifier contract on {chain}."), Style::new().fg(OK).bold()));
                    }
                    ProofMode::Compressed => {
                        border = ACCENT;
                        lines.push(Line::styled(format!("  Done{took}. A real STARK proof was made and checked on this machine."), Style::new().fg(ACCENT).bold()));
                        lines.push(Line::styled("  The contract used the local stand-in verifier: only a Groth16 proof is checked on-chain.", Style::new().fg(DIM)));
                    }
                    ProofMode::Mock => {
                        border = ACCENT;
                        lines.push(Line::styled(format!("  Practice run done{took}. The flow works, but nothing was proven."), Style::new().fg(ACCENT).bold()));
                        lines.push(Line::styled("  SP1 ran every check without making a proof, and the stand-in verifier accepts anyone's.", Style::new().fg(DIM)));
                    }
                }
                lines.push(Line::styled("  All of the above is saved to proofs/receipt.md, with commands to check it without this program.", Style::new().fg(DIM)));
            } else {
                border = Color::Yellow;
            }
        }

        let inner_h = area.height.saturating_sub(2);
        let width = area.width.saturating_sub(2).max(1) as usize;
        let total: u16 = lines
            .iter()
            .map(|l| l.width().max(1).div_ceil(width) as u16)
            .sum();
        let max = total.saturating_sub(inner_h);
        if self.follow {
            self.scroll = max;
        } else {
            self.scroll = self.scroll.min(max);
        }
        let title = if max > 0 {
            format!(" data · {}/{} ", self.scroll.min(max), max)
        } else {
            " data ".to_string()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border))
            .title(title);
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }

    fn draw_help(&self, f: &mut Frame) {
        let full = f.area();
        let (w, h) = (86.min(full.width), 17.min(full.height));
        let area = Rect {
            x: full.x + (full.width - w) / 2,
            y: full.y + (full.height - h) / 2,
            width: w,
            height: h,
        };
        let k = |key: &str, text: &str| {
            Line::from(vec![
                Span::styled(format!("  {key:<7}"), Style::new().fg(Color::White).bold()),
                Span::styled(text.to_string(), Style::new().fg(Color::Gray)),
            ])
        };
        let lines = vec![
            Line::raw(""),
            k("enter", "send your message and run everything"),
            k("e", "write a new message"),
            k(
                "d",
                "details: every step, the exact command it ran, and its output",
            ),
            k("q", "quit"),
            Line::raw(""),
            Line::styled("  Settings", Style::new().fg(ACCENT).bold()),
            k("m", &format!("proof: {}", mode_name(self.settings.mode))),
            Line::styled(
                "         practice: seconds; runs every check, makes no proof: proves nothing",
                Style::new().fg(DIM),
            ),
            Line::styled(
                "         real (STARK): ~5 minutes on a laptop; checked here, not on-chain",
                Style::new().fg(DIM),
            ),
            Line::styled(
                "         real (Groth16): what real chains take; ~10+ min, needs Docker",
                Style::new().fg(DIM),
            ),
            k(
                "s",
                &format!(
                    "validators' signatures from: {}",
                    match self.settings.certs {
                        crate::steps::CertSource::Shreds => "the next block (no plugin needed)",
                        crate::steps::CertSource::Plugin => "the footer-dump plugin",
                    }
                ),
            ),
            Line::raw(""),
            Line::styled("  any key closes this", Style::new().fg(DIM)),
        ];
        f.render_widget(ratatui::widgets::Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(" help ");
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
        ProofMode::Mock => {
            "About 10 seconds as a practice run, which proves nothing (m for a real proof)."
        }
        ProofMode::Compressed => "Takes about 5 minutes: the real proof is the slow part.",
        ProofMode::Groth16 => "Takes 10+ minutes: the real Groth16 proof is the slow part.",
    }
}

fn prove_hint(m: ProofMode) -> &'static str {
    match m {
        ProofMode::Mock => "practice proof: a few seconds",
        ProofMode::Compressed => {
            "a real proof: usually about 5 minutes. Your laptop's fans may spin up."
        }
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

/// What went wrong and what to do, in plain words.
fn failure_help(s: &StepView) -> Vec<String> {
    let Status::Failed(err, _) = &s.status else {
        return vec![];
    };
    let mut out = Vec::new();
    if s.id == StepId::Doctor {
        out.push("Something Huginn needs isn't ready:".to_string());
        let mut lines = s.log.iter().peekable();
        while let Some(l) = lines.next() {
            if let Some(rest) = l.strip_prefix("✗ ") {
                out.push(format!(
                    "  • {}",
                    rest.split_whitespace().collect::<Vec<_>>().join(" ")
                ));
                if let Some(next) = lines.peek() {
                    if next.starts_with("    ") {
                        out.push(format!("    fix: {}", next.trim()));
                    }
                }
            }
        }
    } else {
        out.push(format!(
            "It stopped here: {}",
            err.lines().next().unwrap_or(err)
        ));
    }
    out.push(String::new());
    out.push("Press d to see exactly what happened, then enter to try again.".into());
    out
}

/// One fact as lines: `label  value`, or the label on its own line when the value is long, so a
/// hash always sits on one (soft-wrapped) line that selects and copies cleanly.
fn fact_lines(f: &Fact, width: usize, label_w: usize) -> Vec<Line<'static>> {
    let value_style = if f.value.contains('✓') || f.value.starts_with("REJECTED") {
        Style::new().fg(OK).bold()
    } else if f.value == "NO" || f.value.contains("FAILED") {
        Style::new().fg(BAD).bold()
    } else {
        Style::new().fg(Color::White)
    };
    let mut out = Vec::new();
    if f.value.chars().count() + label_w > width {
        out.push(Line::styled(
            format!("      {}", f.label),
            Style::new().fg(DIM),
        ));
        out.push(Line::styled(format!("        {}", f.value), value_style));
    } else {
        out.push(Line::from(vec![
            Span::styled(
                format!("      {:<w$}", f.label, w = label_w - 6),
                Style::new().fg(DIM),
            ),
            Span::styled(f.value.clone(), value_style),
        ]));
    }
    if let Some(l) = &f.link {
        out.push(Line::styled(
            format!("        {l}"),
            Style::new().fg(Color::Cyan),
        ));
    }
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
    Line::from(vec![
        Span::styled(format!(" {k:<9}"), Style::new().fg(DIM)),
        Span::raw(v),
    ])
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
    use {
        super::*,
        ratatui::{backend::TestBackend, Terminal},
    };

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
            "anvil started, chain 31337",
            "SolanaLightClient at 0xe7f1…",
            "SolanaGreeter at 0x8A79…C318, greeting gm",
            "signature 638WLVFo…",
            "slot 124404 finalized",
            "block_id rebuilt and certified",
            "mock proof in 0.4s, 5122326 cycles",
            "accepted, 105765 gas",
            "isVerified(signature) = true",
            "greeting is now \"hello from solana\"",
            "forged memo rejected, real one accepted",
        ];
        for (i, s) in summaries.iter().enumerate() {
            app.steps[i].status =
                Status::Ok(s.to_string(), Duration::from_millis(300 + 400 * i as u64));
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
        app.view = if std::env::var("VIEW").as_deref() == Ok("details") {
            View::Details
        } else {
            View::Guided
        };
        app.run_took = Some(Duration::from_millis(9800));
        t.draw(|f| app.draw(f)).unwrap();
        let buf = t.backend().buffer();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            println!("{}", line.trim_end());
        }
    }
}

#[cfg(test)]
mod guided_tests {
    use {
        super::*,
        ratatui::{backend::TestBackend, Terminal},
    };

    fn render(app: &mut App, title: &str) {
        let mut t = Terminal::new(TestBackend::new(110, 30)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        println!("===== {title} =====");
        let buf = t.backend().buffer();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
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
        let prove = index(StepId::Prove);
        for i in 0..prove {
            a.steps[i].status = Status::Ok(String::new(), ms(500));
        }
        a.state.slot = Some(126768);
        a.state.tx_ok = Some(true);
        a.steps[prove].status = Status::Running(Instant::now() - ms(2300));
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
        render(&mut a, "done (practice)");

        a.state.proof_mode = Some(ProofMode::Groth16);
        render(&mut a, "done (groth16)");

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

    /// The data feed, with values from a real devnet run (slot 519,666,924).
    #[test]
    fn data_feed() {
        let mut a = app();
        a.settings.cluster = Cluster::Devnet;
        a.settings.mode = ProofMode::Groth16;
        a.state.chain_id = Some(84532);
        let ms = |n| Duration::from_millis(n);
        for id in [
            StepId::Doctor,
            StepId::Anvil,
            StepId::Deploy,
            StepId::Greeter,
            StepId::Send,
            StepId::Finalize,
            StepId::Fetch,
        ] {
            a.steps[index(id)].status = Status::Ok(String::new(), ms(1500));
        }
        a.steps[index(StepId::Prove)].status = Status::Running(Instant::now() - ms(95_000));
        a.steps[index(StepId::Prove)].log =
            vec!["huginn.prove(.., groth16)  (real Groth16, ~10+ min, Docker)".into()];
        a.busy = true;
        let fact = |step: &str, label: &str, value: &str, link: Option<&str>| Fact {
            step: step.into(),
            label: label.into(),
            value: value.into(),
            link: link.map(String::from),
        };
        a.facts = vec![
            fact("send", "paid by", "DKNJmZ85XJkYmR1BFV9qYAqMkNs2ZratnchhWZPimdng", None),
            fact("send", "memo (full)", "huginn:84532:0xc6e7df5e7b4f2a278906862b61205850344d4e7d:1:gm from devnet", None),
            fact("send", "signature", "2A64K4NwakFz1FLKZxyvAPXVbNb9DXs7PiYmMG1vbqfKz6PJUjdMiVM5kwL4rBCcvQQi4dXyYkYZMf3DgJ1EVn8n", Some("https://explorer.solana.com/tx/2A64K4NwakFz1FLKZxyvAPXVbNb9DXs7PiYmMG1vbqfKz6PJUjdMiVM5kwL4rBCcvQQi4dXyYkYZMf3DgJ1EVn8n?cluster=devnet")),
            fact("finalize", "slot", "519,666,924", None),
            fact("finalize", "transaction result", "succeeded", None),
            fact("fetch", "served by (peer, identity)", "64.130.42.134:8009 DhKciBn2jKLtbJm4AwU2qzJBqZsq9KmzJuJU2u2Ks6z7", None),
            fact("fetch", "data shreds", "192 in 6 FEC sets of 32", None),
            fact("fetch", "block_id rebuilt from shreds", "1527711f4168f9ccca2e3673cb55f509df896b9658712f05aa87cc05175c36f3", None),
            fact("fetch", "block_id in the certificate", "1527711f4168f9ccca2e3673cb55f509df896b9658712f05aa87cc05175c36f3", None),
            fact("fetch", "signers", "18 of 22 ranked validators", None),
            fact("fetch", "signed stake", "165,232,440,832,694,625 of 166,380,536,867,236,617 = 99.31%", None),
            fact("prove", "validators in the set", "22", None),
            fact("prove", "cycles: total", "6,002,283", None),
        ];
        a.scroll = 0;
        a.follow = true;
        let mut t = Terminal::new(TestBackend::new(120, 44)).unwrap();
        t.draw(|f| a.draw(f)).unwrap();
        println!("===== data feed (devnet, groth16, proving) =====");
        let buf = t.backend().buffer();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect();
            println!("{}", line.trim_end());
        }
    }
}
