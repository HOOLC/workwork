use zork_gui::api::{AgentStatus, Role, TranscriptMessage};
use zork_gui::components::message::{InlineSegment, MessageBlock, MessageDocument};
use zork_gui::transcript::{
    begin_optimistic_user, prepend_older_lines, project_page_with_pending,
    rollback_optimistic_user, should_render_live_activity, stable_task_title,
    take_pending_user_echo, transcript_line_from, TranscriptLine,
};

fn collect_segments<'a>(blocks: &'a [MessageBlock], output: &mut Vec<&'a InlineSegment>) {
    for block in blocks {
        match block {
            MessageBlock::Paragraph(content) | MessageBlock::Heading { content, .. } => {
                output.extend(content.segments())
            }
            MessageBlock::List { items, .. } => {
                for item in items {
                    collect_segments(&item.blocks, output);
                }
            }
            MessageBlock::BlockQuote(children) => collect_segments(children, output),
            MessageBlock::CodeBlock { .. } | MessageBlock::Rule | MessageBlock::Table { .. } => {}
        }
    }
}

#[test]
fn coding_markdown_becomes_structured_readable_blocks() {
    let source = concat!(
        "# 渲染标题\n\n",
        "普通 **粗体**、*斜体*、~~删除~~、`let x = 1` 和 ",
        "[文档](https://example.com/docs)。\n\n",
        "- first\n- [x] 完成 😀\n\n",
        "> 引用内容\n\n",
        "```rust\nfn main() { println!(\"你好\"); }\n```\n\n",
        "---\n",
    );

    let document = MessageDocument::parse(source);
    let blocks = document.blocks();
    assert!(matches!(
        blocks.first(),
        Some(MessageBlock::Heading { level: 1, .. })
    ));
    assert!(blocks.iter().any(|block| matches!(
        block,
        MessageBlock::CodeBlock { language: Some(language), code }
            if language == "rust" && code.contains("println!")
    )));
    assert!(blocks
        .iter()
        .any(|block| matches!(block, MessageBlock::BlockQuote(_))));
    assert!(blocks
        .iter()
        .any(|block| matches!(block, MessageBlock::Rule)));
    assert!(blocks.iter().any(|block| matches!(
        block,
        MessageBlock::List { items, .. }
            if items.iter().any(|item| item.checked == Some(true))
    )));

    let mut segments = Vec::new();
    collect_segments(blocks, &mut segments);
    assert!(segments
        .iter()
        .any(|segment| segment.text == "粗体" && segment.style.strong));
    assert!(segments
        .iter()
        .any(|segment| segment.text == "斜体" && segment.style.emphasis));
    assert!(segments
        .iter()
        .any(|segment| segment.text == "删除" && segment.style.strikethrough));
    assert!(segments
        .iter()
        .any(|segment| segment.text == "let x = 1" && segment.style.code));
    assert!(segments.iter().any(|segment| {
        segment.text == "文档" && segment.style.link.as_deref() == Some("https://example.com/docs")
    }));
    assert!(document.plain_text().contains("完成 😀"));
}

#[test]
fn incomplete_streaming_markdown_and_long_unicode_remain_readable() {
    let incomplete = "开始 **尚未闭合，`code\n第二行 👨‍👩‍👧‍👦 e\u{301}";
    let parsed = MessageDocument::parse(incomplete).plain_text();
    for expected in ["开始", "尚未闭合", "code", "第二行", "👨‍👩‍👧‍👦", "e\u{301}"]
    {
        assert!(
            parsed.contains(expected),
            "missing {expected:?} from {parsed:?}"
        );
    }

    let long = "长😀e\u{301}".repeat(1_500);
    assert_eq!(MessageDocument::parse(&long).plain_text(), long);
}

#[test]
fn transcript_projection_keeps_gateway_delivered_user_and_assistant_roles() {
    let user = TranscriptMessage::Message {
        role: Role::User,
        content: "你好\nsecond line".to_owned(),
    };
    let assistant = TranscriptMessage::Message {
        role: Role::Assistant,
        content: "**deliberate reply**".to_owned(),
    };

    assert!(matches!(
        transcript_line_from(&user),
        Some(TranscriptLine::Message {
            role: Role::User,
            content
        }) if content == "你好\nsecond line"
    ));
    assert!(matches!(
        transcript_line_from(&assistant),
        Some(TranscriptLine::Message {
            role: Role::Assistant,
            content
        }) if content == "**deliberate reply**"
    ));
}

#[test]
fn optimistic_user_echo_is_one_copy_in_both_event_orders_and_rolls_back() {
    for echo_before_http_completion in [true, false] {
        let mut lines = Vec::new();
        let mut pending = Vec::new();
        begin_optimistic_user(&mut lines, &mut pending, "same text".to_owned());
        assert_eq!(lines.len(), 1);
        assert_eq!(pending, ["same text"]);

        if echo_before_http_completion {
            assert!(take_pending_user_echo(&mut pending, "same text"));
            // A successful HTTP completion must not append another local row.
        } else {
            // A successful HTTP completion is intentionally a no-op; the echo
            // is reconciled afterward.
            assert!(take_pending_user_echo(&mut pending, "same text"));
        }

        assert_eq!(lines.len(), 1);
        assert!(pending.is_empty());
    }

    let mut lines = Vec::new();
    let mut pending = Vec::new();
    begin_optimistic_user(&mut lines, &mut pending, "restore me".to_owned());
    assert!(rollback_optimistic_user(
        &mut lines,
        &mut pending,
        "restore me"
    ));
    assert!(lines.is_empty());
    assert!(pending.is_empty());
}

#[test]
fn reconnect_page_consumes_the_pending_user_echo() {
    let items = vec![TranscriptMessage::Message {
        role: Role::User,
        content: "sent while reconnecting".to_owned(),
    }];
    let mut pending = vec!["sent while reconnecting".to_owned()];

    let lines = project_page_with_pending(&items, &mut pending);

    assert_eq!(lines.len(), 1);
    assert!(pending.is_empty());
}

#[test]
fn older_pages_prepend_in_order_without_repeating_an_overlap() {
    let mut lines = vec![
        TranscriptLine::Message {
            role: Role::User,
            content: "u2".to_owned(),
        },
        TranscriptLine::Message {
            role: Role::Assistant,
            content: "a2".to_owned(),
        },
    ];
    let older = vec![
        TranscriptMessage::Message {
            role: Role::User,
            content: "u1".to_owned(),
        },
        TranscriptMessage::Message {
            role: Role::Assistant,
            content: "a1".to_owned(),
        },
    ];

    assert_eq!(prepend_older_lines(&mut lines, &older), 2);
    assert_eq!(
        lines
            .iter()
            .map(|line| match line {
                TranscriptLine::Message { content, .. } => content.as_str(),
            })
            .collect::<Vec<_>>(),
        ["u1", "a1", "u2", "a2"]
    );
    assert_eq!(prepend_older_lines(&mut lines, &older), 0);
    assert_eq!(lines.len(), 4);
}

#[test]
fn partial_history_never_becomes_a_false_task_title() {
    let lines = vec![TranscriptLine::Message {
        role: Role::User,
        content: "history user message 103".to_owned(),
    }];

    assert_eq!(
        stable_task_title("54d9362eabcd", &lines, true),
        "Task 54d9362e"
    );
    assert_eq!(
        stable_task_title("54d9362eabcd", &lines, false),
        "history user message 103"
    );
}

#[test]
fn live_activity_is_separate_from_messages_and_keeps_waits_and_failures() {
    let last_message = TranscriptLine::Message {
        role: Role::Assistant,
        content: "explicit update".to_owned(),
    };
    assert!(!should_render_live_activity(
        &AgentStatus::Clear,
        Some(&last_message)
    ));
    assert!(should_render_live_activity(
        &AgentStatus::Waiting {
            reason: "job completion".to_owned(),
            deadline_ms: 0,
        },
        Some(&last_message)
    ));
    assert!(should_render_live_activity(
        &AgentStatus::Thinking,
        Some(&last_message)
    ));
    assert!(
        !should_render_live_activity(&AgentStatus::Finished, Some(&last_message)),
        "a successful terminal event must not leave a bare finished row in the conversation"
    );
    assert!(should_render_live_activity(
        &AgentStatus::Failed {
            reason: "model crashed".to_owned(),
        },
        Some(&last_message)
    ));
    assert!(should_render_live_activity(
        &AgentStatus::Interrupted,
        Some(&last_message)
    ));
}
