//! Recover an agent's Markdown destination when a terminal replay kept only the label.
use std::{
    fs::File,
    io::{self, BufRead, BufReader},
    path::Path,
};

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

use super::block::{Block, LineOutcome, Role, parser_for};
use crate::model::AgentKind;

pub(crate) fn url_for_label(
    path: &Path,
    agent: AgentKind,
    label: &str,
) -> io::Result<Option<String>> {
    let mut reader = BufReader::new(File::open(path)?);
    let parse = parser_for(agent);
    let mut found = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(found);
        }
        let Ok(raw) = std::str::from_utf8(&line) else {
            continue;
        };
        if !raw.contains('[') {
            continue;
        }
        let LineOutcome::Emit(message) = parse(raw, 0) else {
            continue;
        };
        if message.role != Role::Assistant {
            continue;
        }
        for block in message.blocks {
            let Block::Text { text } = block else {
                continue;
            };
            let mut link = None;
            for event in Parser::new(&text) {
                match event {
                    Event::Start(Tag::Link { dest_url, .. }) => {
                        link = Some((dest_url, String::new()))
                    }
                    Event::Text(part) | Event::Code(part) => {
                        if let Some((_, title)) = &mut link {
                            title.push_str(&part);
                        }
                    }
                    Event::SoftBreak | Event::HardBreak => {
                        if let Some((_, title)) = &mut link {
                            title.push(' ');
                        }
                    }
                    Event::End(TagEnd::Link) => {
                        if let Some((uri, title)) = link.take()
                            && title == label
                        {
                            if !vt100::is_web_url(&uri)
                                || found
                                    .as_deref()
                                    .is_some_and(|previous| previous != uri.as_ref())
                            {
                                return Ok(None);
                            }
                            found = Some(uri.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn only_unambiguous_assistant_links_can_supply_a_missing_target() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let append = |file: &mut tempfile::NamedTempFile, role: &str, text: &str| {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "response_item", "payload": {
                        "type": "message", "role": role,
                        "content": [{"type": "output_text", "text": text}]
                    }
                })
            )
            .unwrap();
        };
        append(&mut file, "user", "[View source](https://example.org/user)");
        append(
            &mut file,
            "assistant",
            "`[View source](https://example.org/code)`\n\n[View **source**](https://example.org/a_(b))",
        );
        assert_eq!(
            url_for_label(file.path(), AgentKind::Codex, "View source")
                .unwrap()
                .as_deref(),
            Some("https://example.org/a_(b)")
        );
        append(&mut file, "assistant", "[View source](/local-file)");
        assert_eq!(
            url_for_label(file.path(), AgentKind::Codex, "View source").unwrap(),
            None
        );
    }
}
