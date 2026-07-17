use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::ExecutableCommand;
use pirs_agent::{Agent, AgentEvent};
use pirs_ai::Message;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;

use crate::approval::ApprovalMode;

pub struct TuiOptions {
    pub agent: Agent,
    pub host: Option<Arc<pirs_rhai::ExtensionHost>>,
    pub session_path: std::path::PathBuf,
    pub approval_mode: ApprovalMode,
    pub approval_gate: Option<Arc<crate::approval::ApprovalGate>>,
    pub cwd: std::path::PathBuf,
}

struct App {
    lines: Vec<Line<'static>>,
    input: String,
    running: bool,
    live: Option<usize>,
    tick: u64,
    dirty: bool,
    last_live_refresh: std::time::Instant,
    steer_queue: Arc<Mutex<Vec<String>>>,
    /// Lines scrolled up from the bottom (0 = pinned to bottom).
    scroll: u16,
    viewport_height: u16,
    status: String,
    usage_summary: String,
    pending_approval: Arc<Mutex<Option<String>>>,
    approval_answer: Arc<std::sync::mpsc::Sender<String>>,
    cancel: tokio_util::sync::CancellationToken,
}

impl App {
    fn push_line(&mut self, line: Line<'static>) {
        self.lines.push(line);
        self.scroll = 0;
        self.dirty = true;
    }

    fn push_text(&mut self, style: Style, text: impl Into<String>) {
        let text = text.into();
        for (i, l) in text.lines().enumerate() {
            let prefix = if i == 0 { "" } else { "  " };
            self.push_line(Line::from(Span::styled(format!("{prefix}{l}"), style)));
        }
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn tool_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn error_style() -> Style {
    Style::default().fg(Color::Red)
}

fn user_style() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

fn assistant_style() -> Style {
    Style::default().fg(Color::White)
}

pub async fn run(mut opts: TuiOptions) -> anyhow::Result<()> {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    opts.agent.subscribe(Arc::new(move |event: AgentEvent| {
        let _ = event_tx.send(event);
    }));

    let session_file = opts.session_path.clone();
    opts.agent.subscribe(Arc::new(move |event: AgentEvent| {
        if let AgentEvent::MessageEnd { message } = event {
            let _ = crate::session::append(&session_file, &[*message]);
        }
    }));

    let (pending_approval, approval_answer_rx) = approval_bridge(&mut opts);

    let steer_sender = opts.agent.steer_sender();
    let steer_queue: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let steer_queue = Arc::clone(&steer_queue);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let msgs: Vec<String> = steer_queue.lock().unwrap().drain(..).collect();
            for m in msgs {
                steer_sender(Message::user(m));
            }
        });
    }

    let mut terminal = setup_terminal()?;
    let mut app = App {
        lines: Vec::new(),
        input: String::new(),
        running: false,
        live: None,
        tick: 0,
        dirty: true,
        last_live_refresh: std::time::Instant::now(),
        steer_queue,
        scroll: 0,
        viewport_height: 10,
        status: format!(
            "{} | {} | {} | Ctrl-C cancel | Ctrl-D quit",
            opts.agent.model,
            opts.approval_mode.name(),
            opts.cwd
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string())
        ),
        usage_summary: String::new(),
        pending_approval,
        approval_answer: approval_answer_rx,
        cancel: opts.agent.cancel_handle(),
    };
    app.push_text(dim(), "pirs tui — type a message and press Enter");

    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
    let agent = Arc::new(tokio::sync::Mutex::new(opts.agent));
    {
        let agent = Arc::clone(&agent);
        let done_tx = done_tx.clone();
        tokio::spawn(async move {
            while let Some(text) = prompt_rx.recv().await {
                let mut a = agent.lock().await;
                let result = a.prompt(&text).await;
                drop(a);
                let _ = done_tx.send(result.is_ok());
            }
        });
    }

    let mut events = crossterm::event::EventStream::new();
    loop {
        while let Ok(event) = event_rx.try_recv() {
            apply_agent_event(&mut app, event);
        }
        while let Ok(ok) = done_rx.try_recv() {
            app.running = false;
            app.dirty = true;
            let report = {
                let a = agent.lock().await;
                a.usage_report()
            };
            let total = report.grand_total();
            let hit = if total.input + total.cache_read > 0 {
                100.0 * total.cache_read as f64 / (total.input + total.cache_read) as f64
            } else {
                0.0
            };
            app.usage_summary = format!(
                " | in {} ({}% cached) out {}",
                total.input, hit as u64, total.output
            );
            if !ok {
                app.push_text(error_style(), "[run failed]");
            }
        }

        let maybe_event = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            futures::StreamExt::next(&mut events),
        )
        .await;
        if let Ok(Some(Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })))) = maybe_event
        {
            app.dirty = true;
            match (code, modifiers) {
                (KeyCode::Char('d'), KeyModifiers::CONTROL) => break,
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    if app.running {
                        app.cancel.cancel();
                        app.push_text(dim(), "[cancel requested]");
                    } else {
                        break;
                    }
                }
                (KeyCode::Enter, _) => {
                    let text = app.input.trim().to_string();
                    app.input.clear();
                    if !text.is_empty() {
                        let approval_question = app.pending_approval.lock().unwrap().take();
                        if approval_question.is_some() {
                            let _ = app.approval_answer.send(text.clone());
                            continue;
                        }
                        if text == "/quit" {
                            break;
                        }
                        app.push_text(user_style(), format!("> {text}"));
                        if app.running {
                            app.steer_queue.lock().unwrap().push(text);
                        } else {
                            app.running = true;
                            let _ = prompt_tx.send(text);
                        }
                    }
                }
                (KeyCode::Char(c), KeyModifiers::NONE)
                | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                    app.input.push(c);
                }
                (KeyCode::Backspace, _) => {
                    app.input.pop();
                }
                (KeyCode::PageUp, _) => {
                    let max = app
                        .lines
                        .len()
                        .saturating_sub(app.viewport_height as usize)
                        .min(u16::MAX as usize) as u16;
                    app.scroll = (app.scroll + 10).min(max);
                }
                (KeyCode::PageDown, _) => {
                    app.scroll = app.scroll.saturating_sub(10);
                }
                _ => {}
            }
        } else if app.running {
            app.dirty = true;
        }

        if !app.dirty {
            continue;
        }
        app.dirty = false;
        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(3),
                ])
                .split(frame.area());

            let vh = chunks[0].height;
            app.viewport_height = vh;
            let max_top = app.lines.len().saturating_sub(vh as usize);
            let top = max_top.saturating_sub(app.scroll as usize) as u16;
            let text: Vec<Line> = app.lines.clone();
            let convo = Paragraph::new(text)
                .block(Block::default().borders(Borders::TOP).title("pirs"))
                .wrap(Wrap { trim: false })
                .scroll((top, 0));
            frame.render_widget(convo, chunks[0]);

            const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let spinner = if app.running {
                format!(" {} running", FRAMES[(app.tick / 2 % 10) as usize])
            } else {
                String::new()
            };
            app.tick = app.tick.wrapping_add(1);
            let mut status = format!("{}{}{}", app.status, spinner, app.usage_summary);
            status.truncate(chunks[1].width as usize);
            frame.render_widget(
                Paragraph::new(Span::styled(status, dim())),
                chunks[1],
            );

            let (input_title, input_style) = if app.pending_approval.lock().unwrap().is_some() {
                ("approval [y/n/a]", error_style())
            } else {
                ("input (steers while running)", user_style())
            };
            let input = Paragraph::new(app.input.as_str())
                .block(Block::default().borders(Borders::ALL).title(input_title))
                .style(input_style);
            frame.render_widget(input, chunks[2]);
            let cursor_x = (chunks[2].x + 1
                + unicode_width::UnicodeWidthStr::width(app.input.as_str()) as u16)
                .min(chunks[2].x + chunks[2].width.saturating_sub(2));
            frame.set_cursor_position((cursor_x, chunks[2].y + 1));
        })?;
    }


    restore_terminal()?;
    if let Some(h) = &opts.host {
        for err in h.drain_hook_errors() {
            eprintln!("[extension error] {err}");
        }
    }
    Ok(())
}

fn thinking_lines(a: &pirs_ai::AssistantMessage) -> Vec<Line<'static>> {
    const MAX: usize = 10;
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);
    let all: Vec<Line<'static>> = a
        .content
        .iter()
        .filter_map(|b| match b {
            pirs_ai::ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                Some(thinking)
            }
            _ => None,
        })
        .flat_map(|t| {
            t.lines()
                .map(|l| Line::from(Span::styled(format!("thinking: {l}"), style)))
                .collect::<Vec<_>>()
        })
        .collect();
    let all_len = all.len();
    if all_len > MAX {
        let mut out = vec![Line::from(Span::styled(
            format!("thinking: … ({} lines)", all_len),
            dim(),
        ))];
        out.extend(all.into_iter().skip(all_len.saturating_sub(MAX)));
        return out;
    }
    all
}


fn apply_agent_event(app: &mut App, event: AgentEvent) {
    match event {
        AgentEvent::MessageStart { message } => {
            if message.is_assistant() {
                app.live = Some(app.lines.len());
            }
        }
        AgentEvent::MessageUpdate { message } => {
            if let Some(idx) = app.live {
                if app.last_live_refresh.elapsed() < std::time::Duration::from_millis(120) {
                    return;
                }
                app.last_live_refresh = std::time::Instant::now();
                app.lines.truncate(idx);
                let mut preview: Vec<Line> = thinking_lines(&message);
                for l in message.text().lines() {
                    preview.push(Line::from(Span::styled(l.to_string(), assistant_style())));
                }
                if preview.len() > 8 {
                    let start = preview.len() - 8;
                    app.lines.push(Line::from(Span::styled("…", dim())));
                    app.lines.extend(preview.drain(start..));
                } else {
                    app.lines.extend(preview);
                }
                app.scroll = 0;
                app.dirty = true;
            }
        }
        AgentEvent::MessageEnd { message } => {
            if let Message::Assistant(a) = *message {
                if let Some(idx) = app.live.take() {
                    app.lines.truncate(idx);
                }
                for line in thinking_lines(&a) {
                    app.lines.push(line);
                }
                let text = a.text();
                if !text.trim().is_empty() {
                    app.push_text(assistant_style(), text);
                }
                if a.stop_reason == pirs_ai::StopReason::Error {
                    app.push_text(
                        error_style(),
                        format!("[error: {}]", a.error_message.unwrap_or_default()),
                    );
                }
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => {
            let summary = crate::summarize_args(&tool_name, &args);
            app.push_text(tool_style(), format!("> {tool_name} {summary}"));
        }
        AgentEvent::ToolExecutionEnd { result, .. } => {
            let text: String = result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            let preview: String = text.lines().take(3).collect::<Vec<_>>().join("\n");
            if !preview.is_empty() {
                let style = if result.is_error { error_style() } else { dim() };
                app.push_text(style, format!("- {preview}"));
            }
        }
        AgentEvent::CompactionStart { .. } => {
            app.push_text(dim(), "[compacting context...]");
        }
        AgentEvent::CompactionEnd { aborted, .. } => {
            if aborted {
                app.push_text(dim(), "[compaction skipped]");
            } else {
                app.push_text(dim(), "[compaction done]");
            }
        }
        _ => {}
    }
}

fn approval_bridge(
    opts: &mut TuiOptions,
) -> (Arc<Mutex<Option<String>>>, Arc<std::sync::mpsc::Sender<String>>) {
    let pending: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(gate) = &opts.approval_gate {
        let pending = Arc::clone(&pending);
        let rx = Arc::new(std::sync::Mutex::new(rx));
        gate.set_prompter(move |question| {
            *pending.lock().unwrap() = Some(question.to_string());
            rx.lock()
                .unwrap()
                .recv()
                .unwrap_or_else(|_| "n".to_string())
        });
    }
    (pending, Arc::new(tx))
}

fn setup_terminal() -> anyhow::Result<Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal() -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}