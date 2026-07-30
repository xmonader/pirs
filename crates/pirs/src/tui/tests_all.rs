use super::*;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

/// Wraps `ratatui::backend::TestBackend`, counting cursor-escape calls so
/// `draw_with_cursor_dedup`'s dedup behavior can be asserted mechanically
/// (there's no terminal to visually watch blink in a test).
struct CountingBackend {
    inner: ratatui::backend::TestBackend,
    hide_calls: u32,
    show_calls: u32,
    move_calls: u32,
}

impl CountingBackend {
    fn new(w: u16, h: u16) -> Self {
        CountingBackend {
            inner: ratatui::backend::TestBackend::new(w, h),
            hide_calls: 0,
            show_calls: 0,
            move_calls: 0,
        }
    }
}

impl ratatui::backend::Backend for CountingBackend {
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.hide_calls += 1;
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.show_calls += 1;
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        self.move_calls += 1;
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> std::io::Result<()> {
        self.inner.clear()
    }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[test]
fn cursor_dedup_skips_reemission_when_position_is_unchanged() {
    let mut terminal = Terminal::new(CountingBackend::new(20, 5)).unwrap();
    let mut last_cursor = None;

    // Frame 1: cursor appears at (2, 1) — first time, must emit.
    draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
        frame.render_widget(Paragraph::new("hi"), frame.area());
        Some((2, 1))
    })
    .unwrap();
    assert_eq!(terminal.backend().show_calls, 1);
    assert_eq!(terminal.backend().move_calls, 1);

    // Frame 2: content changes (simulating streamed tokens) but the
    // cursor position is identical — must NOT re-emit.
    draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
        frame.render_widget(Paragraph::new("hi there, more text"), frame.area());
        Some((2, 1))
    })
    .unwrap();
    assert_eq!(
        terminal.backend().show_calls,
        1,
        "unchanged cursor position must not re-trigger Show"
    );
    assert_eq!(
        terminal.backend().move_calls,
        1,
        "unchanged cursor position must not re-trigger MoveTo"
    );

    // Frame 3: cursor actually moves — must emit again.
    draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
        frame.render_widget(Paragraph::new("hi there"), frame.area());
        Some((5, 1))
    })
    .unwrap();
    assert_eq!(terminal.backend().show_calls, 2);
    assert_eq!(terminal.backend().move_calls, 2);

    // Frame 4: cursor hidden entirely — must emit hide.
    draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
        frame.render_widget(Paragraph::new("hi there"), frame.area());
        None
    })
    .unwrap();
    assert_eq!(terminal.backend().hide_calls, 1);

    // Frame 5: still hidden — production re-emits Hide every frame so
    // ratatui MoveTo diffs cannot leave a visible caret on the status line.
    draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
        frame.render_widget(Paragraph::new("hi there"), frame.area());
        None
    })
    .unwrap();
    assert_eq!(
        terminal.backend().hide_calls,
        2,
        "hidden cursor must re-emit Hide each frame (see draw_with_cursor_dedup)"
    );
}

#[test]
fn wrap_words_respects_width() {
    let lines = wrap_words("hello beautiful world", 10);
    assert!(lines
        .iter()
        .all(|l| unicode_width::UnicodeWidthStr::width(l.as_str()) <= 10));
    assert!(lines.len() >= 2);
}

#[test]
fn wrap_words_empty() {
    assert_eq!(wrap_words("", 10), vec![""]);
}

#[test]
fn truncate_chars_shortens() {
    let s = truncate_chars("abcdefghijklmnopqrstuvwxyz", 10);
    assert!(s.chars().count() <= 10);
    assert!(s.ends_with('…'));
}

#[test]
fn inline_spans_code_and_bold() {
    let theme = Theme::default_dark();
    let spans = inline_spans("use `foo` and **bar** ok", &theme);
    let joined: String = spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(joined, "use foo and bar ok");
    assert!(spans.len() >= 5);
}

#[test]
fn render_markdown_code_fence() {
    let theme = Theme::default_dark();
    let md = "before\n```rust\nfn main() {}\n```\nafter";
    let lines = render_markdown(md, &theme, 80);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("fn main()"));
    assert!(text.contains("╭─ rust") || text.contains("rust"));
}

#[test]
fn compact_num_formats() {
    assert_eq!(compact_num(42), "42");
    assert_eq!(compact_num(1500), "1.5k");
    assert_eq!(compact_num(2_500_000), "2.5M");
}

#[test]
fn cursor_pos_multiline() {
    assert_eq!(cursor_pos("hi", 80), (2, 0));
    assert_eq!(cursor_pos("hi\nthere", 80), (5, 1));
}

#[test]
fn chat_item_user_renders_label() {
    let theme = Theme::default_dark();
    let lines = ChatItem::User("hello".into()).render(&theme, 80, false);
    let flat: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect::<Vec<_>>()
        .join("|");
    assert!(flat.contains("you"));
    assert!(flat.contains("hello"));
}

#[test]
fn clear_chat_keeps_items_and_caches_in_sync() {
    let mut app = test_app();
    for i in 0..5 {
        app.push(ChatItem::Notice(format!("n{i}")));
    }
    assert_eq!(app.items.len(), 5);
    assert_eq!(app.item_caches.len(), 5);
    app.clear_chat();
    assert_eq!(app.items.len(), 1, "notice after clear");
    assert_eq!(
        app.item_caches.len(),
        app.items.len(),
        "caches must stay length-aligned with items"
    );
    assert!(matches!(app.items[0], ChatItem::Notice(_)));
}

#[test]
fn tool_quiet_read_collapses_on_success() {
    let theme = Theme::default_dark();
    let item = ChatItem::ToolCall {
        name: "read".into(),
        summary: "src/main.rs".into(),
        preview: "fn main() {}\n// more\n// lines".into(),
        is_error: false,
        done: true,
        expanded: false,
    };
    let lines = item.render(&theme, 80, false);
    assert_eq!(lines.len(), 1, "collapsed read is header-only: {lines:?}");
    let flat: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(flat.contains("Read"), "{flat}");
    assert!(flat.contains("src/main.rs"), "{flat}");
}

#[test]
fn tool_bash_expanded_shows_l_border_body() {
    let theme = Theme::default_dark();
    let item = ChatItem::ToolCall {
        name: "bash".into(),
        summary: "cargo test".into(),
        preview: "ok\npassed".into(),
        is_error: false,
        done: true,
        expanded: true,
    };
    let lines = item.render(&theme, 80, false);
    assert!(lines.len() > 1, "bash body should expand: {lines:?}");
    let flat: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(flat.contains("Ran") || flat.contains("bash"), "{flat}");
    assert!(flat.contains("⎢") || flat.contains("⎣"), "{flat}");
}

#[test]
fn thinking_can_collapse_and_expand() {
    let theme = Theme::default_dark();
    let item = ChatItem::Assistant {
        thinking: "line1\nline2\nline3".into(),
        text: "hi".into(),
        error: None,
    };
    let collapsed = item.render(&theme, 80, false);
    let flat: String = collapsed
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(flat.contains("thought"), "{flat}");
    assert!(flat.contains("3 lines"), "{flat}");
    assert!(
        !flat.contains("line1"),
        "body hidden when collapsed: {flat}"
    );

    let expanded = item.render(&theme, 80, true);
    let flat_e: String = expanded
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(flat_e.contains("line1"), "{flat_e}");
    assert!(flat_e.contains("thought"), "{flat_e}");
    assert!(flat_e.contains("t hide"), "{flat_e}");
}

#[test]
fn thinking_live_collapsed_is_stable_one_line() {
    let theme = Theme::default_dark();
    let body_a = (0..10)
        .map(|i| format!("step {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let body_b = (0..30)
        .map(|i| format!("step {i} more words here"))
        .collect::<Vec<_>>()
        .join("\n");
    let flat = |body: &str, n: usize| {
        let lines = render_thinking_live(body, &theme, false);
        assert_eq!(
            lines.len(),
            1,
            "collapsed live thinking is a single status line"
        );
        let s: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(s.contains("thinking"), "{s}");
        assert!(s.contains(&format!("{n} line")), "{s}");
        assert!(s.contains("t expand"), "{s}");
        // No streaming CoT snippet — that rewrote the line end every token.
        assert!(
            !s.contains("step "),
            "collapsed must not embed CoT text: {s}"
        );
        s
    };
    let a = flat(&body_a, 10);
    let b = flat(&body_b, 30);
    // Shape stable: only the count digit region differs.
    assert!(a.starts_with("    · thinking · "), "{a}");
    assert!(b.starts_with("    · thinking · "), "{b}");
}

#[test]
fn thinking_live_expanded_shows_tail() {
    let theme = Theme::default_dark();
    let body = (0..30)
        .map(|i| format!("step {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let lines = render_thinking_live(&body, &theme, true);
    let flat: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(flat.contains("thinking"), "{flat}");
    assert!(
        flat.contains("step 29"),
        "must show latest when expanded: {flat}"
    );
    assert!(flat.contains("t collapse"), "{flat}");
}

#[test]
fn composer_title_does_not_call_default_auto_yolo() {
    // Default approval "auto" must not paint as the yolo badge.
    pirs_tools::with_live_permission_mode(pirs_tools::PermissionMode::WorkspaceWrite, || {
        assert_eq!(composer_title("auto", false, false), " edit ");
        assert_ne!(composer_title("auto", false, false), " yolo ");
    });
    pirs_tools::with_live_permission_mode(pirs_tools::PermissionMode::DangerFullAccess, || {
        assert_eq!(composer_title("yolo", false, false), " full ");
    });
    pirs_tools::with_live_permission_mode(pirs_tools::PermissionMode::ReadOnly, || {
        assert_eq!(composer_title("auto", false, false), " plan ");
        assert_eq!(composer_title("auto", true, false), " running ");
    });
}

#[test]
fn extract_think_tags_from_text() {
    let s = extract_think_tags("before <think>\nhmm\n</think> after").unwrap();
    assert!(s.contains("hmm"));
}

#[test]
fn tool_verb_and_default_expand_policy() {
    assert_eq!(tool_verb("bash", false), "Running");
    assert_eq!(tool_verb("bash", true), "Ran");
    assert_eq!(tool_verb("read", true), "Read");
    assert!(!tool_default_expanded("read", false));
    assert!(tool_default_expanded("bash", false));
    assert!(tool_default_expanded("read", true)); // errors expand
}

#[test]
fn approval_grace_blocks_until_elapsed() {
    assert!(approval_grace_elapsed(None));
    let recent = Some(std::time::Instant::now());
    assert!(!approval_grace_elapsed(recent));
    let old = Some(std::time::Instant::now() - std::time::Duration::from_millis(500));
    assert!(approval_grace_elapsed(old));
}

#[test]
fn composer_mode_styles_differ_for_yolo_and_plan() {
    // composer_mode_style consults live_permission_mode() first — pin
    // workspace-write so approval_mode string colors are what we assert.
    // with_live_permission_mode gates concurrent suite tests on the same slot.
    pirs_tools::with_live_permission_mode(pirs_tools::PermissionMode::WorkspaceWrite, || {
        let theme = Theme::default_dark();
        let idle = composer_mode_style(&theme, "ask", false, false);
        let yolo = composer_mode_style(&theme, "yolo", false, false);
        let plan = composer_mode_style(&theme, "plan", false, false);
        let pending = composer_mode_style(&theme, "ask", false, true);
        assert_ne!(yolo.fg, idle.fg);
        assert_ne!(plan.fg, yolo.fg);
        assert_eq!(pending.fg, theme.approval.fg);
    });
}

#[test]
fn finish_tool_updates_open_card_in_place() {
    let mut app = test_app();
    app.start_tool("read".into(), "a.rs".into());
    assert_eq!(app.items.len(), 1);
    app.finish_tool("read", "contents".into(), false);
    assert_eq!(app.items.len(), 1, "must not push a second card");
    match &app.items[0] {
        ChatItem::ToolCall {
            done,
            expanded,
            preview,
            ..
        } => {
            assert!(*done);
            assert!(!*expanded, "quiet read stays collapsed");
            assert_eq!(preview, "contents");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn quiet_tools_collapse_into_verb_group() {
    let mut app = test_app();
    for path in ["a.rs", "b.rs", "c.rs"] {
        app.start_tool("read".into(), path.into());
        app.finish_tool("read", format!("// {path}"), false);
    }
    assert_eq!(
        app.items.len(),
        1,
        "three reads → one group: {:?}",
        app.items
    );
    match &app.items[0] {
        ChatItem::ToolGroup {
            name,
            members,
            expanded,
        } => {
            assert_eq!(name, "read");
            assert_eq!(members.len(), 3);
            assert!(!*expanded);
        }
        other => panic!("expected ToolGroup, got {other:?}"),
    }
    let theme = Theme::default_dark();
    let lines = app.items[0].render(&theme, 80, false);
    let flat: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(flat.contains("Read 3 files"), "{flat}");
}

#[test]
fn edit_preview_uses_diff_colors() {
    let theme = Theme::default_dark();
    let item = ChatItem::ToolCall {
        name: "edit".into(),
        summary: "x.rs".into(),
        preview: " context\n-old\n+new\n".into(),
        is_error: false,
        done: true,
        expanded: true,
    };
    let lines = item.render(&theme, 80, false);
    // Find styled + / - lines
    let mut saw_plus = false;
    let mut saw_minus = false;
    for line in &lines {
        for span in &line.spans {
            if span.content.contains("+new") {
                saw_plus = true;
                assert_eq!(span.style.fg, theme.success.fg);
            }
            if span.content.contains("-old") {
                saw_minus = true;
                assert_eq!(span.style.fg, theme.tool_err.fg);
            }
        }
    }
    assert!(
        saw_plus && saw_minus,
        "diff lines should be present: {lines:?}"
    );
}

#[test]
fn slash_filter_matches_prefix() {
    let ext = vec![
        ("goal".into(), "show or set session goal".into()),
        ("btw".into(), "side question".into()),
    ];
    let m = slash_filter("/mo", &ext);
    assert!(m.iter().any(|c| c.name == "/model"), "{m:?}");
    assert!(!m.iter().any(|c| c.name == "/quit"));
    let goal = slash_filter("/go", &ext);
    assert!(goal.iter().any(|c| c.name == "/goal"), "{goal:?}");
    let btw = slash_filter("/bt", &ext);
    assert!(btw.iter().any(|c| c.name == "/btw"), "{btw:?}");
    let all = slash_filter("/", &ext);
    assert!(all.len() >= 10);
    assert!(all.iter().any(|c| c.name == "/goal"), "{all:?}");
    assert!(all.iter().any(|c| c.name == "/btw"), "{all:?}");
}

#[test]
fn slash_completion_applies_selected() {
    let mut app = test_app();
    app.input = "/mo".into();
    app.cursor = 3;
    app.slash_sel = 0;
    apply_slash_completion(&mut app);
    assert!(app.input.starts_with("/model"), "{}", app.input);
    assert!(app.input.ends_with(' '));
}

#[test]
fn starter_fills_input() {
    let mut app = test_app();
    app.apply_starter(0);
    assert!(app.input.contains("repository"));
    assert_eq!(app.cursor, app.input.len());
}

#[test]
fn welcome_first_run_mentions_starters() {
    let theme = Theme::default_dark();
    let item = ChatItem::Welcome {
        model: "m".into(),
        plan_model: None,
        strategy: None,
        approval: "ask".into(),
        cwd: "proj".into(),
        first_run: true,
    };
    let lines = item.render(&theme, 80, false);
    let flat: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(flat.contains("Getting started"), "{flat}");
    assert!(flat.contains("Explain this repo"), "{flat}");
}

#[test]
fn wrap_line_to_rows_wraps_long_line() {
    let theme = Theme::default_dark();
    let long = "word ".repeat(40); // ~200 cols with spaces
    let line = Line::from(Span::styled(long, theme.assistant_text));
    let rows = wrap_line_to_rows(&line, 20);
    assert!(rows.len() > 1);
    assert!(rows.iter().all(|r| line_width(r) <= 20));
}

#[test]
fn wrap_line_to_rows_fast_path_when_fits() {
    let theme = Theme::default_dark();
    let line = Line::from(Span::styled("short".to_string(), theme.assistant_text));
    assert_eq!(wrap_line_to_rows(&line, 20).len(), 1);
}

#[test]
fn wrap_line_hard_splits_overlong_token() {
    let theme = Theme::default_dark();
    let line = Line::from(Span::styled("x".repeat(50), theme.assistant_text));
    let rows = wrap_line_to_rows(&line, 10);
    assert!(rows.len() >= 5);
    assert!(rows.iter().all(|r| line_width(r) <= 10));
}

#[test]
fn wrap_line_preserves_span_styles() {
    let theme = Theme::default_dark();
    let line = Line::from(vec![
        Span::styled("aaaa bbbb ".to_string(), theme.accent),
        Span::styled("cccc dddd eeee".to_string(), theme.error),
    ]);
    let rows = wrap_line_to_rows(&line, 8);
    // Every span in every row keeps one of the two original styles.
    for r in &rows {
        for s in &r.spans {
            assert!(s.style == theme.accent || s.style == theme.error);
        }
    }
}

#[test]
fn split_keep_spaces_alternates() {
    assert_eq!(split_keep_spaces("ab  cd"), vec!["ab", "  ", "cd"]);
    assert_eq!(split_keep_spaces("  x"), vec!["  ", "x"]);
    assert_eq!(split_keep_spaces("x"), vec!["x"]);
}

#[test]
fn clip_spans_bounds_width_and_keeps_style() {
    let theme = Theme::default_dark();
    let spans = vec![
        Span::styled("hello ".to_string(), theme.accent),
        Span::styled("world!!!".to_string(), theme.error),
    ];
    let clipped = clip_spans(spans, 8);
    let w: usize = clipped
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    assert!(w <= 8);
    assert_eq!(clipped[0].style, theme.accent);
}

#[test]
fn flatten_rows_all_within_width() {
    let theme = Theme::default_dark();
    let lines = vec![
        Line::from(Span::styled("short".to_string(), theme.assistant_text)),
        Line::from(Span::styled("a ".repeat(30), theme.assistant_text)),
    ];
    let rows = flatten_rows(&lines, 16);
    assert!(rows.len() >= 3);
    assert!(rows.iter().all(|r| line_width(r) <= 16));
}

#[test]
fn tool_status_glyphs() {
    assert_eq!(tool_status_glyph(false, false, 0).0, "○");
    assert_eq!(tool_status_glyph(true, false, 0).0, "✓");
    assert_eq!(tool_status_glyph(true, true, 0).0, "✗");
}

#[test]
fn latest_slot_overwrites_unconsumed_value_rather_than_queuing() {
    // The core backpressure behavior: pushing B then C before anyone
    // takes A's successor must leave only C, not queue B behind A.
    let slot: LatestSlot<u32> = LatestSlot::new();
    slot.push(1);
    assert_eq!(slot.take_blocking(), Some(1));
    slot.push(2);
    slot.push(3);
    assert_eq!(
        slot.take_blocking(),
        Some(3),
        "second push should replace, not queue behind, the first"
    );
}

#[test]
fn push_coalesce_appends_pending_frame_deltas_instead_of_dropping() {
    // Regression: ratatui hands the writer incremental diffs. If a pending
    // delta is replaced instead of appended while the writer is mid-flush,
    // the cells it painted (e.g. keystrokes typed during token streaming)
    // are lost for good -> garbled input line. Coalescing must preserve the
    // earlier delta's bytes, in order, ahead of the newer one.
    let slot: LatestSlot<Vec<u8>> = LatestSlot::new();
    slot.push_coalesce(b"AB".to_vec());
    slot.push_coalesce(b"CD".to_vec());
    assert_eq!(
        slot.take_blocking(),
        Some(b"ABCD".to_vec()),
        "unconsumed frame deltas must concatenate in push order, not drop"
    );
    // Once drained, the next delta stands alone again.
    slot.push_coalesce(b"EF".to_vec());
    assert_eq!(slot.take_blocking(), Some(b"EF".to_vec()));
}

#[test]
fn latest_slot_wakes_a_blocked_consumer() {
    // A consumer waiting on an empty slot must be woken by a later push,
    // not just see a value that was already there when it started.
    let slot = Arc::new(LatestSlot::<u32>::new());
    let consumer_slot = Arc::clone(&slot);
    let handle = std::thread::spawn(move || consumer_slot.take_blocking());

    std::thread::sleep(std::time::Duration::from_millis(50));
    slot.push(42);

    assert_eq!(handle.join().unwrap(), Some(42));
}

#[test]
fn latest_slot_close_releases_a_blocked_consumer_with_none() {
    let slot = Arc::new(LatestSlot::<u32>::new());
    let consumer_slot = Arc::clone(&slot);
    let handle = std::thread::spawn(move || consumer_slot.take_blocking());

    std::thread::sleep(std::time::Duration::from_millis(50));
    slot.close();

    assert_eq!(handle.join().unwrap(), None);
}

#[test]
fn latest_slot_close_still_yields_a_value_pushed_before_it() {
    let slot: LatestSlot<u32> = LatestSlot::new();
    slot.push(7);
    slot.close();
    assert_eq!(
        slot.take_blocking(),
        Some(7),
        "a value pushed before close must still be delivered"
    );
    assert_eq!(
        slot.take_blocking(),
        None,
        "nothing left after that -> consumer should stop"
    );
}

#[test]
fn tui_writer_push_and_shutdown_do_not_hang() {
    // Doesn't assert on stdout content (nothing to intercept without
    // redesigning TuiWriter around a generic writer), but proves the
    // full spawn -> push -> shutdown lifecycle actually terminates
    // rather than leaving the background thread parked forever.
    let writer = TuiWriter::spawn();
    writer.push(b"hello".to_vec());
    writer.push(b"world".to_vec());
    writer.shutdown();
}

#[test]
fn last_assistant_text_finds_newest_reply() {
    let items = vec![
        ChatItem::User("hi".into()),
        ChatItem::Assistant {
            thinking: String::new(),
            text: "first".into(),
            error: None,
        },
        ChatItem::User("again".into()),
        ChatItem::Assistant {
            thinking: "t".into(),
            text: "  second reply  \n".into(),
            error: None,
        },
        ChatItem::Notice("n".into()),
    ];
    assert_eq!(last_assistant_text(&items).as_deref(), Some("second reply"));
    assert_eq!(last_assistant_text(&[]), None);
    assert_eq!(last_assistant_text(&[ChatItem::User("only".into())]), None);
}

#[test]
fn mouse_capture_off_by_default_for_native_select() {
    // Production default must leave mouse free so terminal select/copy works.
    std::env::remove_var("PIRS_TUI_MOUSE");
    assert!(
        !mouse_capture_enabled(),
        "default must not capture mouse (blocks native selection)"
    );
    std::env::set_var("PIRS_TUI_MOUSE", "1");
    assert!(mouse_capture_enabled());
    std::env::remove_var("PIRS_TUI_MOUSE");
}

/// TUI session-end and `/stats` must consume `report_pins()` via the shared
/// session_stats pin APIs (same contract as one-shot/REPL).
#[test]
fn tui_production_exits_use_report_pins_api() {
    // Scan production modules (split layout): app owns report_pins; mod/slash_exec call pin APIs.
    let prod = concat!(
        include_str!("mod.rs"),
        include_str!("app.rs"),
        include_str!("slash_exec.rs"),
    );
    assert!(
        prod.contains("fn report_pins"),
        "App must expose report_pins snapshot"
    );
    assert!(
        prod.contains("print_session_stats_pins"),
        "TUI session-end must call print_session_stats_pins"
    );
    assert!(
        prod.contains("format_session_stats_pins"),
        "TUI /stats must call format_session_stats_pins"
    );
    assert!(
        prod.contains("app.report_pins()"),
        "TUI exits must build pins from app.report_pins()"
    );
}

/// Live app pin snapshot drives the shipped session-stats hybrid formatter.
#[test]
fn tui_report_pins_drive_hybrid_session_stats() {
    use pirs_ai::Usage;
    let mut app = test_app();
    app.model = "weak-executor".into();
    app.plan_model = Some("strong-planner".into());
    app.strategy = Some("plan-exec".into());
    let pins = app.report_pins();
    assert_eq!(pins.plan_model(), Some("strong-planner"));
    assert_eq!(pins.strategy(), Some("plan-exec"));

    let mut report = pirs_agent::usage::UsageReport::default();
    report.calls.push(pirs_agent::usage::UsageRecord {
        model: "strong-planner".into(),
        usage: Usage {
            input: 800,
            output: 100,
            total_tokens: 900,
            ..Default::default()
        },
        stop_reason: pirs_ai::StopReason::Stop,
        timestamp: 0,
    });
    report.calls.push(pirs_agent::usage::UsageRecord {
        model: "weak-executor".into(),
        usage: Usage {
            input: 400,
            output: 50,
            total_tokens: 450,
            ..Default::default()
        },
        stop_reason: pirs_ai::StopReason::Stop,
        timestamp: 1,
    });
    *report.by_model.entry("strong-planner".into()).or_default() = Usage {
        input: 800,
        output: 100,
        total_tokens: 900,
        ..Default::default()
    };
    *report.by_model.entry("weak-executor".into()).or_default() = Usage {
        input: 400,
        output: 50,
        total_tokens: 450,
        ..Default::default()
    };

    let text = session_stats::format_session_stats_pins(&app.clock, &report, &app.model, &pins);
    assert!(
        text.contains("by role"),
        "TUI hybrid stats must include by role:\n{text}"
    );
    assert!(
        text.contains("planner") && text.contains("executor"),
        "{text}"
    );
    assert!(
        text.contains("strong-planner") && text.contains("weak-executor"),
        "{text}"
    );
    assert!(text.contains("plan-exec"), "{text}");
}

/// A minimal but fully valid `App`, for tests that need to drive
/// `draw_chat` directly rather than just its pure helper functions.
fn test_app() -> App {
    App {
        items: Vec::new(),
        live: None,
        input: String::new(),
        cursor: 0,
        history: Vec::new(),
        history_idx: None,
        history_draft: String::new(),
        running: false,
        tick: 0,
        dirty: true,
        last_live_refresh: std::time::Instant::now(),
        steer_queue: Arc::new(Mutex::new(Vec::new())),
        scroll: 0,
        viewport_height: 10,
        model: "test-model".into(),
        plan_model: None,
        strategy: None,
        model_aliases: Vec::new(),
        approval_mode: "auto".into(),
        session_path: PathBuf::from("/tmp/test-session.jsonl"),
        cwd: PathBuf::from("."),
        cwd_label: ".".into(),
        usage_summary: String::new(),
        pending_approval: Arc::new(Mutex::new(None)),
        approval_answer: Arc::new(std::sync::mpsc::channel().0),
        approval_opened_at: None,
        cancel: Arc::new(Mutex::new(tokio_util::sync::CancellationToken::new())),
        show_help: false,
        model_picker: None,
        status_msg: String::new(),
        last_activity: String::new(),
        turn_started_at: None,
        // Show reasoning live + in history by default; user can hide with t.
        thinking_expanded: false,
        slash_sel: 0,
        ext_slash: Vec::new(),
        first_run_session: false,
        should_quit: false,
        item_caches: Vec::new(),
        cache_width: 0,
        total_rows: 0,
        last_draw_width: 0,
        desired_cursor: None,
        clock: SessionClock::new(),
    }
}

fn draw_chat_once(app: &mut App, width: u16, height: u16) -> ratatui::backend::TestBackend {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::default_dark();
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw_chat(frame, area, app, &theme);
        })
        .unwrap();
    terminal.backend().clone()
}

fn backend_text(backend: &ratatui::backend::TestBackend) -> String {
    backend
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect::<String>()
}

#[test]
fn draw_chat_pinned_to_bottom_shows_newest_items_and_evicts_far_off_screen() {
    let mut app = test_app();
    for i in 0..2000 {
        app.push(ChatItem::Notice(format!("item-{i:04}")));
    }
    // Pinned to the bottom (scroll == 0): the newest items should be
    // visible, and the oldest ones — far from the viewport — should not
    // still be holding exact rows in memory after this draw.
    let backend = draw_chat_once(&mut app, 40, 5);
    let text = backend_text(&backend);
    assert!(text.contains("item-1999"), "{text}");
    assert!(
        !text.contains("item-0000"),
        "the oldest item shouldn't be in a 5-row viewport pinned to the bottom: {text}"
    );
    assert!(
        app.item_caches[0].rows.is_none(),
        "item far from the viewport should have its exact rows evicted"
    );
    assert!(
        app.item_caches[1999].rows.is_some(),
        "item actually painted must have exact rows cached"
    );
}

#[test]
fn draw_chat_scrolled_to_top_measures_top_items_and_evicts_the_bottom() {
    let mut app = test_app();
    for i in 0..2000 {
        app.push(ChatItem::Notice(format!("item-{i:04}")));
    }
    // First draw pinned to the bottom, so the tail is measured/cached...
    draw_chat_once(&mut app, 40, 5);
    assert!(app.item_caches[1999].rows.is_some());

    // ...then scroll all the way to the top and redraw.
    app.scroll = u16::MAX;
    let backend = draw_chat_once(&mut app, 40, 5);
    let text = backend_text(&backend);
    assert!(text.contains("item-0000"), "{text}");
    assert!(
        app.item_caches[0].rows.is_some(),
        "now-visible top item must be measured"
    );
    assert!(
        app.item_caches[1999].rows.is_none(),
        "no-longer-visible bottom item should have been evicted: far from the new viewport"
    );
}

#[test]
fn draw_chat_resize_remeasures_items_at_the_new_width() {
    let mut app = test_app();
    // Long enough to wrap differently at 60 cols vs 20.
    app.push(ChatItem::Notice("word ".repeat(30)));
    draw_chat_once(&mut app, 60, 20);
    let rows_at_60 = app.item_caches[0].row_count;
    assert!(app.item_caches[0].rows.is_some());

    draw_chat_once(&mut app, 20, 20);
    let rows_at_20 = app.item_caches[0].row_count;
    assert!(
        rows_at_20 > rows_at_60,
        "the same text should wrap into more rows at a narrower width: \
             {rows_at_20} rows at 20 cols vs {rows_at_60} at 60 cols"
    );
    assert!(
        app.item_caches[0].rows.is_some(),
        "the item is in view at both widths, so it should be re-measured, not left stale"
    );
}

#[test]
fn draw_chat_new_items_are_placeholders_until_actually_drawn() {
    let mut app = test_app();
    app.push(ChatItem::Notice("only item".into()));
    assert!(
        app.item_caches[0].rows.is_none(),
        "App::push has no width/theme to measure with"
    );
    draw_chat_once(&mut app, 40, 10);
    assert!(
        app.item_caches[0].rows.is_some(),
        "the first draw after a push should measure it once it's in view"
    );
}
