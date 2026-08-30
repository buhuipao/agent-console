use std::io::Write;

use serde_json::json;
use tempfile::tempdir;

use super::{
    block::{Block, Role, SUMMARY_LIMIT},
    *,
};

fn transcript(
    name: &str,
    records: &[serde_json::Value],
) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempdir().unwrap();
    let path = root.path().join(name);
    let mut file = fs::File::create(&path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
    (root, path)
}

fn claude_page(records: &[serde_json::Value]) -> MessagePage {
    let (_root, path) = transcript("session.jsonl", records);
    read_page(&path, AgentKind::Claude, Position::Tail, DEFAULT_LIMIT).unwrap()
}

fn codex_page(records: &[serde_json::Value]) -> MessagePage {
    let (_root, path) = transcript("rollout.jsonl", records);
    read_page(&path, AgentKind::Codex, Position::Tail, DEFAULT_LIMIT).unwrap()
}

#[test]
fn a_claude_assistant_turn_keeps_thinking_text_and_tool_calls_in_order() {
    let page = claude_page(&[json!({
        "type": "assistant",
        "uuid": "assistant-1",
        "isSidechain": false,
        "timestamp": "2026-08-21T15:03:24.906Z",
        "message": {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "the test is flaky", "signature": "EqkFCqUBCBEYAipA"},
            {"type": "text", "text": "I'll run the suite first."},
            {"type": "tool_use", "id": "toolu_1", "name": "Bash",
             "input": {"command": "cargo test --locked", "description": "run tests"}}
        ]}
    })]);

    let message = &page.messages[0];
    assert_eq!(message.id, "assistant-1");
    assert_eq!(message.role, Role::Assistant);
    assert_eq!(message.ts, 1_787_324_604);
    assert_eq!(
        message.blocks,
        vec![
            Block::Thinking {
                text: "the test is flaky".into()
            },
            Block::Text {
                text: "I'll run the suite first.".into()
            },
            Block::ToolUse {
                id: "toolu_1".into(),
                name: "Bash".into(),
                summary: "cargo test --locked".into(),
            },
        ],
        "the opaque `signature` and the tool's secondary input fields are not conversation"
    );
}

#[test]
fn a_failed_claude_tool_result_is_reported_as_not_ok() {
    let page = claude_page(&[json!({
        "type": "user",
        "uuid": "user-1",
        "timestamp": "2026-08-21T15:03:34.635Z",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true,
             "content": "error: could not compile `agent-console`"},
            {"type": "tool_result", "tool_use_id": "toolu_2",
             "content": [{"type": "text", "text": "207 passed"}]}
        ]}
    })]);

    assert_eq!(
        page.messages[0].blocks,
        vec![
            Block::ToolResult {
                tool_use_id: "toolu_1".into(),
                ok: false,
                summary: "error: could not compile `agent-console`".into(),
            },
            Block::ToolResult {
                tool_use_id: "toolu_2".into(),
                ok: true,
                summary: "207 passed".into(),
            },
        ]
    );
}

#[test]
fn oversized_tool_output_is_truncated_with_its_original_size() {
    let output = "line of build output\n".repeat(4_000);
    let page = claude_page(&[json!({
        "type": "user",
        "uuid": "user-1",
        "timestamp": "2026-08-21T15:03:34.635Z",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": output}
        ]}
    })]);

    let Block::ToolResult { summary, .. } = &page.messages[0].blocks[0] else {
        panic!("expected a tool result, got {:?}", page.messages[0].blocks);
    };
    assert!(
        summary.contains(&format!("[truncated, {} bytes total]", output.trim().len())),
        "the client must be told how much output it is not seeing"
    );
    assert!(summary.len() < SUMMARY_LIMIT * 2, "summary was not capped");
}

#[test]
fn a_base64_image_is_announced_without_relaying_its_payload() {
    let page = claude_page(&[json!({
        "type": "user",
        "uuid": "user-1",
        "timestamp": "2026-08-21T23:46:10.197Z",
        "message": {"role": "user", "content": [
            {"type": "text", "text": "what is wrong with this screen?"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                         "data": "iVBORw0KGgo".repeat(50_000)}}
        ]}
    })]);

    assert_eq!(
        page.messages[0].blocks,
        vec![
            Block::Text {
                text: "what is wrong with this screen?".into()
            },
            Block::Image,
        ]
    );
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(
        !encoded.contains("iVBORw0KGgo"),
        "image bytes must never reach the browser"
    );
}

#[test]
fn sidechain_subagent_turns_stay_out_of_the_main_conversation() {
    let page = claude_page(&[
        json!({"type": "user", "uuid": "main-1", "isSidechain": false,
               "timestamp": "2026-08-21T15:03:20.435Z",
               "message": {"role": "user", "content": "review the web module"}}),
        json!({"type": "user", "uuid": "side-1", "isSidechain": true,
               "timestamp": "2026-08-21T15:03:21.435Z",
               "message": {"role": "user", "content": "You are a subagent. Review src/web."}}),
        json!({"type": "assistant", "uuid": "side-2", "isSidechain": true,
               "timestamp": "2026-08-21T15:03:22.435Z",
               "message": {"role": "assistant", "content": [{"type": "text", "text": "subagent findings"}]}}),
    ]);

    let ids = page
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["main-1"]);
}

#[test]
fn injected_reminders_and_bookkeeping_lines_are_filtered_out_of_claude() {
    let page = claude_page(&[
        json!({"type": "user", "uuid": "noise-1", "timestamp": "2026-08-21T15:03:20.435Z",
               "message": {"role": "user", "content": "<system-reminder>\nremember the plan\n</system-reminder>"}}),
        json!({"type": "user", "uuid": "noise-2", "timestamp": "2026-08-21T15:03:21.435Z",
               "message": {"role": "user", "content": "<task-notification>\n<summary>check-in</summary>\n</task-notification>"}}),
        json!({"type": "last-prompt", "uuid": "noise-3", "lastPrompt": "review the web module"}),
        json!({"type": "custom-title", "uuid": "noise-4", "customTitle": "web ui"}),
        json!({"type": "system", "uuid": "noise-5", "subtype": "turn_duration",
               "timestamp": "2026-08-21T15:03:22.435Z"}),
        json!({"type": "queue-operation", "uuid": "noise-6"}),
        json!({"type": "user", "uuid": "real-1", "timestamp": "2026-08-21T15:03:23.435Z",
               "message": {"role": "user", "content": "please fix the failing test"}}),
    ]);

    assert_eq!(page.messages.len(), 1, "{:?}", page.messages);
    assert_eq!(
        page.messages[0].blocks,
        vec![Block::Text {
            text: "please fix the failing test".into()
        }]
    );
}

#[test]
fn a_typed_slash_command_survives_the_noise_filter() {
    let page = claude_page(&[json!({
        "type": "user", "uuid": "user-1", "timestamp": "2026-08-21T15:03:20.435Z",
        "message": {"role": "user", "content":
            "<command-name>/goal</command-name>\n<command-message>goal</command-message>\n<command-args>ship the web UI</command-args>"}
    })]);

    assert_eq!(
        page.messages[0].blocks,
        vec![Block::Text {
            text: "/goal ship the web UI".into()
        }]
    );
}

#[test]
fn codex_developer_instructions_and_stream_echoes_never_reach_the_conversation() {
    let page = codex_page(&[
        json!({"type": "session_meta", "timestamp": "2026-08-21T04:55:14.866Z",
               "payload": {"session_id": "abc", "cwd": "/repo"}}),
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:15.000Z",
               "payload": {"type": "message", "id": "msg_developer", "role": "developer",
                           "content": [{"type": "input_text", "text": "<skills_instructions>\n## Skills"}]}}),
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:16.000Z",
               "payload": {"type": "message", "id": "msg_user", "role": "user",
                           "content": [{"type": "input_text", "text": "fix the release script"}]}}),
        // The live-stream echo of the same user turn: rendering it too would double every line.
        json!({"type": "event_msg", "timestamp": "2026-08-21T04:55:16.000Z",
               "payload": {"type": "user_message", "message": "fix the release script"}}),
        json!({"type": "world_state", "timestamp": "2026-08-21T04:55:17.000Z",
               "payload": {"full": true, "state": {"agents_md": {"text": "rules"}}}}),
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:18.000Z",
               "payload": {"type": "message", "id": "msg_agent", "role": "assistant",
                           "content": [{"type": "output_text", "text": "On it."}]}}),
    ]);

    let rendered = page
        .messages
        .iter()
        .map(|message| (message.role, message.blocks.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        rendered,
        vec![
            (
                Role::User,
                vec![Block::Text {
                    text: "fix the release script".into()
                }]
            ),
            (
                Role::Assistant,
                vec![Block::Text {
                    text: "On it.".into()
                }]
            ),
        ]
    );
}

#[test]
fn codex_tool_calls_are_summarised_by_the_shell_they_ran() {
    let page = codex_page(&[
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:26.467Z",
               "payload": {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1",
                           "name": "exec",
                           "input": "const r = await tools.shell_command({command:\"cargo clippy --locked\",\"workdir\":\"/repo\"}); text(r);\n"}}),
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:26.594Z",
               "payload": {"type": "custom_tool_call_output", "id": "ctco_1", "call_id": "call_1",
                           "output": [{"type": "input_text", "text": "Script failed\nWall time 0.1 seconds"}]}}),
    ]);

    assert_eq!(
        page.messages[0].blocks,
        vec![Block::ToolUse {
            id: "call_1".into(),
            name: "exec".into(),
            summary: "cargo clippy --locked".into(),
        }]
    );
    assert_eq!(
        page.messages[1].blocks,
        vec![Block::ToolResult {
            tool_use_id: "call_1".into(),
            ok: false,
            summary: "Script failed\nWall time 0.1 seconds".into(),
        }]
    );
}

#[test]
fn a_codex_compaction_does_not_replay_history_the_transcript_already_holds() {
    let history = [
        json!({"type": "message", "id": "msg_1", "role": "user",
               "content": [{"type": "input_text", "text": "add the web UI"}]}),
        json!({"type": "message", "id": "msg_2", "role": "assistant",
               "content": [{"type": "output_text", "text": "starting on it"}]}),
    ];
    let page = codex_page(&[
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:15.000Z",
               "payload": history[0]}),
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:16.000Z",
               "payload": history[1]}),
        json!({"type": "compacted", "timestamp": "2026-08-21T04:55:17.000Z",
               "payload": {"message": "", "replacement_history": history}}),
        json!({"type": "event_msg", "timestamp": "2026-08-21T04:55:17.000Z",
               "payload": {"type": "context_compacted"}}),
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:18.000Z",
               "payload": {"type": "message", "id": "msg_3", "role": "user",
                           "content": [{"type": "input_text", "text": "now add the tests"}]}}),
    ]);

    let ids = page
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["msg_1", "msg_2", "msg_3"],
        "the condensed copy repeats msg_1/msg_2 and must not be rendered a second time"
    );
}

#[test]
fn a_compaction_is_rendered_when_it_is_the_only_history_the_window_can_see() {
    let page = codex_page(&[
        json!({"type": "compacted", "timestamp": "2026-08-21T04:55:17.000Z",
               "payload": {"message": "", "replacement_history": [
                   json!({"type": "message", "id": "msg_1", "role": "user",
                          "content": [{"type": "input_text", "text": "add the web UI"}]})]}}),
        json!({"type": "response_item", "timestamp": "2026-08-21T04:55:18.000Z",
               "payload": {"type": "message", "id": "msg_3", "role": "user",
                           "content": [{"type": "input_text", "text": "now add the tests"}]}}),
    ]);

    let ids = page
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["msg_1", "msg_3"]);
}

#[test]
fn a_cursor_returns_only_what_was_appended_after_it() {
    let (_root, path) = transcript(
        "session.jsonl",
        &[
            json!({"type": "user", "uuid": "user-1", "timestamp": "2026-08-21T15:03:20.435Z",
                 "message": {"role": "user", "content": "first"}}),
        ],
    );

    let first = read_page(&path, AgentKind::Claude, Position::Tail, DEFAULT_LIMIT).unwrap();
    assert_eq!(first.messages.len(), 1);

    // Polling an unchanged transcript costs one stat and returns nothing.
    let idle = read_page(
        &path,
        AgentKind::Claude,
        Position::After(&first.cursor),
        DEFAULT_LIMIT,
    )
    .unwrap();
    assert!(idle.messages.is_empty());
    assert_eq!(idle.cursor, first.cursor);
    assert!(!idle.has_more);

    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(
        file,
        "{}",
        json!({"type": "assistant", "uuid": "assistant-1", "timestamp": "2026-08-21T15:03:21.435Z",
               "message": {"role": "assistant", "content": [{"type": "text", "text": "second"}]}})
    )
    .unwrap();

    let next = read_page(
        &path,
        AgentKind::Claude,
        Position::After(&first.cursor),
        DEFAULT_LIMIT,
    )
    .unwrap();
    let ids = next
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["assistant-1"],
        "the first message must not repeat"
    );
    assert!(next.cursor != first.cursor);
}

#[test]
fn a_half_written_final_line_is_left_for_the_next_poll() {
    let (_root, path) = transcript(
        "session.jsonl",
        &[
            json!({"type": "user", "uuid": "user-1", "timestamp": "2026-08-21T15:03:20.435Z",
                 "message": {"role": "user", "content": "first"}}),
        ],
    );
    let first = read_page(&path, AgentKind::Claude, Position::Tail, DEFAULT_LIMIT).unwrap();

    let complete = json!({"type": "assistant", "uuid": "assistant-1",
                          "timestamp": "2026-08-21T15:03:21.435Z",
                          "message": {"role": "assistant", "content": [{"type": "text", "text": "second"}]}})
    .to_string();
    let (head, tail) = complete.split_at(complete.len() / 2);
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(head.as_bytes()).unwrap();
    drop(file);

    let partial = read_page(
        &path,
        AgentKind::Claude,
        Position::After(&first.cursor),
        DEFAULT_LIMIT,
    )
    .unwrap();
    assert!(
        partial.messages.is_empty(),
        "a line still being written must not be parsed"
    );
    assert_eq!(partial.cursor, first.cursor);

    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{tail}").unwrap();
    drop(file);

    let completed = read_page(
        &path,
        AgentKind::Claude,
        Position::After(&first.cursor),
        DEFAULT_LIMIT,
    )
    .unwrap();
    let ids = completed
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["assistant-1"]);
}

#[test]
fn a_fresh_read_returns_the_tail_of_the_conversation_not_its_head() {
    let records = (0..10)
        .map(|index| {
            json!({"type": "user", "uuid": format!("user-{index}"),
                   "timestamp": "2026-08-21T15:03:20.435Z",
                   "message": {"role": "user", "content": format!("prompt {index}")}})
        })
        .collect::<Vec<_>>();
    let (_root, path) = transcript("session.jsonl", &records);

    let page = read_page(&path, AgentKind::Claude, Position::Tail, 3).unwrap();
    let ids = page
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["user-7", "user-8", "user-9"]);
    assert!(
        !page.has_more,
        "nothing is forward of a tail cursor, so a poller must not spin"
    );

    // And the cursor still sits past everything scanned, including the trimmed head.
    let next = read_page(&path, AgentKind::Claude, Position::After(&page.cursor), 3).unwrap();
    assert!(next.messages.is_empty());
}

#[test]
fn a_client_far_behind_catches_up_from_the_oldest_unseen_message() {
    let (_root, path) = transcript(
        "session.jsonl",
        &[
            json!({"type": "user", "uuid": "user-0", "timestamp": "2026-08-21T15:03:20.435Z",
                 "message": {"role": "user", "content": "prompt 0"}}),
        ],
    );
    let first = read_page(&path, AgentKind::Claude, Position::Tail, 2).unwrap();

    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    for index in 1..6 {
        writeln!(
            file,
            "{}",
            json!({"type": "user", "uuid": format!("user-{index}"),
                   "timestamp": "2026-08-21T15:03:20.435Z",
                   "message": {"role": "user", "content": format!("prompt {index}")}})
        )
        .unwrap();
    }
    drop(file);

    let page = read_page(&path, AgentKind::Claude, Position::After(&first.cursor), 2).unwrap();
    let ids = page
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["user-1", "user-2"]);
    assert!(
        page.has_more,
        "three more messages are forward of this cursor"
    );

    let page = read_page(&path, AgentKind::Claude, Position::After(&page.cursor), 2).unwrap();
    let ids = page
        .messages
        .iter()
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["user-3", "user-4"]);
}

#[test]
fn a_stale_or_bogus_cursor_falls_back_to_the_tail_instead_of_failing() {
    let (_root, path) = transcript(
        "session.jsonl",
        &[
            json!({"type": "user", "uuid": "user-1", "timestamp": "2026-08-21T15:03:20.435Z",
                 "message": {"role": "user", "content": "first"}}),
        ],
    );

    for cursor in ["v1.999999999", "not-a-cursor", ""] {
        let page = read_page(
            &path,
            AgentKind::Claude,
            Position::After(cursor),
            DEFAULT_LIMIT,
        )
        .unwrap();
        let ids = page
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["user-1"],
            "cursor {cursor:?} should reset to the tail"
        );
    }
}

#[test]
fn a_message_serializes_to_the_shape_the_web_ui_expects() {
    let page = claude_page(&[json!({
        "type": "assistant", "uuid": "assistant-1", "timestamp": "2026-08-21T15:03:24.906Z",
        "message": {"role": "assistant", "content": [
            {"type": "text", "text": "done"},
            {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
            {"type": "image", "source": {"data": "AAAA"}}
        ]}
    })]);
    let value = serde_json::to_value(&page).unwrap();

    assert_eq!(value["messages"][0]["role"], "assistant");
    assert_eq!(value["messages"][0]["ts"], 1_787_324_604u64);
    assert_eq!(value["messages"][0]["blocks"][0]["type"], "text");
    assert_eq!(value["messages"][0]["blocks"][1]["type"], "tool_use");
    assert_eq!(value["messages"][0]["blocks"][1]["name"], "Bash");
    assert_eq!(value["messages"][0]["blocks"][1]["summary"], "ls");
    assert_eq!(value["messages"][0]["blocks"][2], json!({"type": "image"}));
    assert!(value["cursor"].is_string());
    assert_eq!(value["has_more"], false);
}

/// Builds `count` Claude turns, each padded so the transcript spans several read windows.
fn padded_transcript(count: usize, padding: usize) -> (tempfile::TempDir, std::path::PathBuf) {
    let records = (0..count)
        .map(|index| {
            json!({"type": "user", "uuid": format!("user-{index:03}"),
                   "timestamp": "2026-08-21T15:03:20.435Z",
                   "padding": "p".repeat(padding),
                   "message": {"role": "user", "content": format!("prompt {index}")}})
        })
        .collect::<Vec<_>>();
    transcript("session.jsonl", &records)
}

fn ids(page: &MessagePage) -> Vec<String> {
    page.messages.iter().map(|m| m.id.clone()).collect()
}

/// Walks from the tail to the start of the file the way the UI's "load earlier" button does,
/// and hands back the whole conversation in order.
fn page_all_backwards(path: &std::path::Path, limit: usize) -> (Vec<String>, usize) {
    let mut page = read_page(path, AgentKind::Claude, Position::Tail, limit).unwrap();
    let mut collected = ids(&page);
    let mut requests = 1;
    while page.has_more_before {
        assert!(requests < 200, "paging backwards is not terminating");
        page = read_page(
            path,
            AgentKind::Claude,
            Position::Before(&page.start_cursor),
            limit,
        )
        .unwrap();
        let mut older = ids(&page);
        older.extend(collected);
        collected = older;
        requests += 1;
    }
    (collected, requests)
}

#[test]
fn paging_backwards_across_several_windows_reassembles_the_whole_conversation() {
    // 60 turns x ~40 KB is comfortably more than one read window, so the walk has to cross a
    // window boundary -- landing mid-line at the leading edge -- to get to the start.
    let (_root, path) = padded_transcript(60, 40 * 1024);
    assert!(
        fs::metadata(&path).unwrap().len() > WINDOW,
        "the fixture has to outgrow a single window for this test to mean anything"
    );

    let (collected, requests) = page_all_backwards(&path, 7);

    let expected = (0..60).map(|i| format!("user-{i:03}")).collect::<Vec<_>>();
    assert_eq!(collected, expected, "took {requests} requests");
    assert!(
        requests > 1,
        "a single page cannot have covered 60 messages at limit 7"
    );
}

#[test]
fn reaching_the_start_of_the_file_stops_offering_earlier_pages() {
    let (_root, path) = padded_transcript(5, 0);

    let (collected, _) = page_all_backwards(&path, 2);
    assert_eq!(collected.len(), 5);

    // The last page of the walk reported no more history; asking anyway terminates cleanly.
    let first = read_page(&path, AgentKind::Claude, Position::Tail, 100).unwrap();
    assert!(!first.has_more_before, "five short turns fit in one window");
    let before_start = read_page(
        &path,
        AgentKind::Claude,
        Position::Before(&first.start_cursor),
        100,
    )
    .unwrap();
    assert!(before_start.messages.is_empty());
    assert!(
        !before_start.has_more_before,
        "byte 0 is the end of the walk"
    );
    assert_eq!(before_start.start_cursor, "v1.0");
}

#[test]
fn an_earlier_page_is_strictly_older_than_the_page_it_came_from() {
    let (_root, path) = padded_transcript(10, 0);

    let tail = read_page(&path, AgentKind::Claude, Position::Tail, 4).unwrap();
    assert_eq!(ids(&tail), ["user-006", "user-007", "user-008", "user-009"]);
    assert!(tail.has_more_before);

    let earlier = read_page(
        &path,
        AgentKind::Claude,
        Position::Before(&tail.start_cursor),
        4,
    )
    .unwrap();
    assert_eq!(
        ids(&earlier),
        ["user-002", "user-003", "user-004", "user-005"],
        "an earlier page is the run immediately before, still oldest-first"
    );
    assert!(
        !earlier.has_more,
        "everything past an earlier page is history the client already holds"
    );
    assert!(
        earlier.has_more_before,
        "two turns are still unread above it"
    );
}

#[test]
fn a_backward_window_that_lands_mid_line_drops_the_partial_record() {
    let (_root, path) = transcript(
        "session.jsonl",
        &[
            json!({"type": "user", "uuid": "user-0", "timestamp": "2026-08-21T15:03:20.435Z",
                   "message": {"role": "user", "content": "first"}}),
            json!({"type": "user", "uuid": "user-1", "timestamp": "2026-08-21T15:03:21.435Z",
                   "message": {"role": "user", "content": "second"}}),
            json!({"type": "user", "uuid": "user-2", "timestamp": "2026-08-21T15:03:22.435Z",
                   "message": {"role": "user", "content": "third"}}),
        ],
    );
    let text = fs::read_to_string(&path).unwrap();
    let second_line_start = text.find('\n').unwrap() as u64 + 1;
    let mid_second_line = second_line_start + 20;
    let length = text.len() as u64;

    let scanned = scan(
        &path,
        AgentKind::Claude,
        mid_second_line,
        true,
        Some(length),
    )
    .unwrap();

    let scanned_ids = scanned
        .messages
        .iter()
        .map(|m| m.message.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        scanned_ids,
        vec!["user-2"],
        "the half of `user-1` inside the window is not a record and must not be emitted"
    );
    assert_eq!(
        scanned.start, mid_second_line,
        "the raw window start is what a backward walk falls back to, so it must not move"
    );
}

#[test]
fn a_bogus_backward_cursor_falls_back_to_the_tail() {
    let (_root, path) = padded_transcript(3, 0);

    let page = read_page(
        &path,
        AgentKind::Claude,
        Position::Before("not-a-cursor"),
        DEFAULT_LIMIT,
    )
    .unwrap();
    assert_eq!(ids(&page), ["user-000", "user-001", "user-002"]);
}

#[test]
fn every_page_reports_both_edges_so_a_client_can_walk_either_way() {
    let (_root, path) = padded_transcript(4, 0);
    let page = read_page(&path, AgentKind::Claude, Position::Tail, 2).unwrap();
    let value = serde_json::to_value(&page).unwrap();

    for field in ["cursor", "start_cursor"] {
        assert!(
            value[field].as_str().is_some_and(|c| c.starts_with("v1.")),
            "{field} must be an opaque cursor: {value}"
        );
    }
    assert!(value["has_more"].is_boolean());
    assert!(value["has_more_before"].is_boolean());
    assert_ne!(
        value["cursor"], value["start_cursor"],
        "the two edges of a non-empty page are different offsets"
    );
}

fn pi_page(records: &[serde_json::Value]) -> MessagePage {
    let (_root, path) = transcript("2026-08-30T00-04-14-153Z_session.jsonl", records);
    read_page(&path, AgentKind::Pi, Position::Tail, DEFAULT_LIMIT).unwrap()
}

#[test]
fn a_pi_assistant_turn_keeps_thinking_text_and_tool_calls_in_order() {
    let page = pi_page(&[json!({
        "type": "message",
        "id": "8ef03f8e",
        "parentId": "df299118",
        "timestamp": "2026-08-30T00:04:16.039Z",
        "message": {"role": "assistant", "provider": "deepseek", "model": "deepseek-v4-flash",
            "stopReason": "toolUse", "content": [
            {"type": "thinking", "thinking": "the test is flaky",
             "thinkingSignature": "reasoning_content"},
            {"type": "text", "text": "I'll run the suite first."},
            {"type": "toolCall", "id": "call_1", "name": "bash",
             "arguments": {"command": "cargo test --locked"}}
        ]}
    })]);

    let message = &page.messages[0];
    assert_eq!(message.id, "8ef03f8e");
    assert_eq!(message.role, Role::Assistant);
    assert_eq!(message.ts, 1_788_048_256);
    assert_eq!(
        message.blocks,
        vec![
            Block::Thinking {
                text: "the test is flaky".into()
            },
            Block::Text {
                text: "I'll run the suite first.".into()
            },
            Block::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                summary: "cargo test --locked".into()
            },
        ]
    );
}

/// pi answers a tool call with a `toolResult` message of its own rather than folding the
/// answer into the next user turn, which is what lets the block carry the id of the call it
/// answers without the reader pairing them up itself.
#[test]
fn a_pi_tool_result_is_its_own_message_and_names_the_call_it_answers() {
    let page = pi_page(&[json!({
        "type": "message",
        "id": "c3d4e5f6",
        "parentId": "8ef03f8e",
        "timestamp": "2026-08-30T00:04:17.000Z",
        "message": {"role": "toolResult", "toolCallId": "call_1", "toolName": "bash",
            "isError": true, "content": [{"type": "text", "text": "1 test failed"}]}
    })]);

    assert_eq!(
        page.messages[0].blocks,
        vec![Block::ToolResult {
            tool_use_id: "call_1".into(),
            ok: false,
            summary: "1 test failed".into()
        }]
    );
}

/// `!command` runs outside the model. Showing it as the command plus its output, rather than
/// as a tool the assistant chose, is the difference between reading the transcript as what
/// happened and reading it as what the agent decided.
#[test]
fn a_pi_user_bash_line_reads_as_a_command_and_its_output() {
    let page = pi_page(&[json!({
        "type": "message",
        "id": "b1",
        "parentId": null,
        "timestamp": "2026-08-30T00:04:18.000Z",
        "message": {"role": "bashExecution", "command": "git status --short",
            "output": " M src/pty.rs", "exitCode": 0, "cancelled": false, "truncated": false}
    })]);

    assert_eq!(
        page.messages[0].blocks,
        vec![
            Block::Text {
                text: "!git status --short".into()
            },
            Block::ToolResult {
                tool_use_id: String::new(),
                ok: true,
                summary: " M src/pty.rs".trim().into()
            },
        ]
    );
}

/// Entry types that are pi's own bookkeeping never reach the conversation: nobody typed them
/// and nobody read them.
#[test]
fn pi_bookkeeping_entries_stay_out_of_the_conversation() {
    let page = pi_page(&[
        json!({"type": "session", "version": 3, "id": "uuid",
               "timestamp": "2026-08-30T00:04:14.153Z", "cwd": "/repo"}),
        json!({"type": "model_change", "id": "aa", "parentId": null,
               "timestamp": "2026-08-30T00:04:14.188Z",
               "provider": "deepseek", "modelId": "deepseek-v4-flash"}),
        json!({"type": "thinking_level_change", "id": "bb", "parentId": "aa",
               "timestamp": "2026-08-30T00:04:14.188Z", "thinkingLevel": "high"}),
        json!({"type": "label", "id": "cc", "parentId": "bb",
               "timestamp": "2026-08-30T00:04:14.200Z",
               "targetId": "aa", "label": "checkpoint-1"}),
        json!({"type": "session_info", "id": "dd", "parentId": "cc",
               "timestamp": "2026-08-30T00:04:14.200Z", "name": "Wire up pi"}),
        json!({"type": "custom", "id": "ee", "parentId": "dd",
               "timestamp": "2026-08-30T00:04:14.300Z",
               "customType": "my-extension", "data": {"count": 42}}),
        json!({"type": "message", "id": "ff", "parentId": "ee",
               "timestamp": "2026-08-30T00:04:14.400Z",
               "message": {"role": "user", "content": [{"type": "text", "text": "Add pi"}]}}),
    ]);

    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].id, "ff");
    assert_eq!(page.messages[0].role, Role::User);
}

/// `/tree` moves the leaf onto another branch and attaches a summary of the one it left. That
/// branch is not in this file's history above the entry, so the summary is context the reader
/// has no other way to get -- unlike a compaction, which only restates what is already there.
#[test]
fn a_pi_branch_summary_is_shown_where_a_compaction_would_be_suppressed() {
    let page = pi_page(&[
        json!({"type": "message", "id": "u1", "parentId": null,
               "timestamp": "2026-08-30T00:40:01.000Z",
               "message": {"role": "user", "content": [{"type": "text", "text": "Try approach A"}]}}),
        json!({"type": "compaction", "id": "k1", "parentId": "u1",
               "timestamp": "2026-08-30T00:40:03.000Z",
               "summary": "Condensed: approach A was explored.", "tokensBefore": 50000}),
        json!({"type": "branch_summary", "id": "g1", "parentId": "k1",
               "timestamp": "2026-08-30T00:40:08.000Z", "fromId": "u1",
               "summary": "The abandoned branch tried a fixed delay."}),
    ]);

    let rendered = page
        .messages
        .iter()
        .map(|message| (message.id.as_str(), &message.blocks))
        .collect::<Vec<_>>();
    assert_eq!(rendered.len(), 2, "{rendered:?}");
    assert_eq!(rendered[0].0, "u1");
    assert_eq!(
        rendered[1],
        (
            "g1",
            &vec![Block::Text {
                text: "The abandoned branch tried a fixed delay.".into()
            }]
        )
    );
}

/// pi leaves `exitCode` out entirely when a command is cancelled or killed, so treating an
/// absent one as zero reported a killed command as a success.
#[test]
fn a_cancelled_pi_shell_command_does_not_read_as_a_success() {
    let cancelled = pi_page(&[json!({
        "type": "message", "id": "b1", "parentId": null,
        "timestamp": "2026-08-30T00:04:18.000Z",
        "message": {"role": "bashExecution", "command": "sleep 100",
            "output": "", "cancelled": true, "truncated": false}
    })]);
    assert_eq!(
        cancelled.messages[0].blocks[1],
        Block::ToolResult {
            tool_use_id: String::new(),
            ok: false,
            summary: String::new()
        }
    );

    let failed = pi_page(&[json!({
        "type": "message", "id": "b2", "parentId": null,
        "timestamp": "2026-08-30T00:04:19.000Z",
        "message": {"role": "bashExecution", "command": "false",
            "output": "", "exitCode": 1, "cancelled": false, "truncated": false}
    })]);
    assert!(matches!(
        failed.messages[0].blocks[1],
        Block::ToolResult { ok: false, .. }
    ));

    let ok = pi_page(&[json!({
        "type": "message", "id": "b3", "parentId": null,
        "timestamp": "2026-08-30T00:04:20.000Z",
        "message": {"role": "bashExecution", "command": "true",
            "output": "done", "exitCode": 0, "cancelled": false, "truncated": false}
    })]);
    assert!(matches!(
        ok.messages[0].blocks[1],
        Block::ToolResult { ok: true, .. }
    ));
}
