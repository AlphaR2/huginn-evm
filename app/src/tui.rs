//! The Huginn TUI: the flow's steps on the left, the selected step's explanation and live
//! log on the right, what the flow has proven so far across the top.

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

struct App {
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
            Event::Finished => self.busy = false,
        }
    }

    /// Runs `ids` in order on the runtime, stopping at the first failure.
    fn start(&mut self, ids: Vec<StepId>, rt: &Runtime, runner: &Arc<Mutex<Runner>>) {
        if self.busy || ids.is_empty() {
            return;
        }
        self.busy = true;
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
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
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
        let [header, flow, body, settings, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(5),
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
        ];
        let proof = vec![
            kv("mode", s.proof_mode.map(|m| m.as_str().to_string()).unwrap_or("—".into())),
            kv("cycles", s.cycles.map(fmt_cycles).unwrap_or("—".into())),
            kv("time", s.prove_secs.map(|t| format!("{t:.1}s")).unwrap_or("—".into())),
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
            ProofMode::Groth16 => "real Groth16, Docker, needs SP1_VERIFIER",
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
            &[("enter", "run all"), ("r", "run step"), ("n", "run from here"), ("m", "proof mode"), ("s", "cert source"), ("e", "edit memo"), ("↑↓", "select"), ("J/K", "scroll log"), ("q", "quit")]
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, v) in keys {
            spans.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(Color::Gray)));
            spans.push(Span::styled(format!(" {v}  "), Style::new().fg(DIM)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).alignment(Alignment::Left), area);
    }
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
            "forged memo rejected, real one accepted",
        ];
        for (i, s) in summaries.iter().enumerate() {
            app.steps[i].status = Status::Ok(s.to_string(), Duration::from_millis(300 + 400 * i as u64));
        }
        app.steps[9].log = vec![
            "1) the real output with its proof:".into(),
            "$ cast call 0xe7f1… verify(bytes,bytes) 0x01b1f252… 0x1d44d290… --rpc-url http://127.0.0.1:8545".into(),
            "   accepted".into(),
            "2) the same proof, memo's last character changed 'e' -> 'd':".into(),
            "$ cast call 0xe7f1… verify(bytes,bytes) 0x01b1f252… 0x1d44d290… --rpc-url http://127.0.0.1:8545".into(),
            "   rejected: InvalidProof".into(),
        ];
        app.list.select(Some(9));
        let mut t = Terminal::new(TestBackend::new(150, 34)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let buf = t.backend().buffer();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
    }
}
