//! Reads a provider transcript as a conversation, cheaply enough to poll.
//!
//! Transcripts are append-only JSONL and routinely hundreds of megabytes, so nothing here
//! parses a file from byte 0. Every page carries two opaque byte-offset cursors -- one at each
//! edge -- and a follow-up request seeks straight to the one it wants and parses a bounded
//! window from there. A request with neither reads a window off the tail, because the bottom
//! of a conversation is the part anyone wants to see first.

mod block;
mod claude;
mod codex;
mod timestamp;

use std::{
    fs,
    io::{self, BufRead, BufReader, Seek, SeekFrom},
    path::Path,
};

use block::{LineOutcome, Message, parser_for};
use serde::Serialize;

use crate::model::AgentKind;

/// Bytes scanned for one page. Big enough to hold a normal stretch of conversation, small
/// enough that opening a 600 MB rollout stays instant.
const WINDOW: u64 = 2 * 1024 * 1024;

/// Retry window for the pathological case: a single transcript line can be a multi-megabyte
/// base64 image, which can swallow the whole first window and yield no messages at all.
const WIDE_WINDOW: u64 = 16 * 1024 * 1024;

/// Above this much unread tail, a forward cursor is treated as too far behind to catch up line
/// by line and the reader falls back to a tail window.
const MAX_INCREMENTAL: u64 = 16 * 1024 * 1024;

pub(crate) const DEFAULT_LIMIT: usize = 50;
pub(crate) const MAX_LIMIT: usize = 500;

const CURSOR_PREFIX: &str = "v1.";

/// Which page of the conversation to read. The three cases are mutually exclusive by
/// construction, which is how `before` and `after` are kept from being combined.
#[derive(Debug)]
pub(crate) enum Position<'a> {
    /// The newest messages. What a client asks for when it opens a conversation.
    Tail,
    /// Newer than this cursor. The polling path.
    After(&'a str),
    /// Older than this cursor. The "load earlier" path.
    Before(&'a str),
}

/// One page of conversation. `messages` is always ordered oldest to newest, whichever
/// direction the page was read in.
#[derive(Serialize)]
pub(crate) struct MessagePage {
    /// The newer edge of this page. Pass as `?after=` to continue forwards.
    pub cursor: String,
    /// The older edge of this page. Pass as `?before=` to continue backwards.
    pub start_cursor: String,
    /// True only when this page was cut short by `limit`, so `?after=cursor` is guaranteed to
    /// return messages this page does not have. Never set merely because the transcript ends
    /// in a half-written line, which would spin a client that loops while it is true.
    pub has_more: bool,
    /// True when older messages exist before `start_cursor`, so the UI should keep offering
    /// "load earlier". Requesting `?before=start_cursor` always moves strictly backwards, so
    /// following it repeatedly terminates at the start of the file.
    pub has_more_before: bool,
    pub messages: Vec<Message>,
}

impl MessagePage {
    /// The empty conversation: a session whose provider has not written a transcript yet.
    pub(crate) fn empty() -> Self {
        Self {
            cursor: encode_cursor(0),
            start_cursor: encode_cursor(0),
            has_more: false,
            has_more_before: false,
            messages: Vec::new(),
        }
    }
}

/// Reads one page of conversation. An unparseable or stale cursor (one past the end of a
/// truncated file) falls back to the tail rather than failing the request.
pub(crate) fn read_page(
    path: &Path,
    agent: AgentKind,
    position: Position<'_>,
    limit: usize,
) -> io::Result<MessagePage> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let length = fs::metadata(path)?.len();
    match position {
        Position::After(cursor) => match decode_cursor(cursor).filter(|start| *start <= length) {
            Some(start) => read_after(path, agent, start, limit, length),
            None => read_tail(path, agent, limit, length),
        },
        Position::Before(cursor) => match decode_cursor(cursor) {
            Some(boundary) => read_before(path, agent, boundary.min(length), limit),
            None => read_tail(path, agent, limit, length),
        },
        Position::Tail => read_tail(path, agent, limit, length),
    }
}

fn read_after(
    path: &Path,
    agent: AgentKind,
    start: u64,
    limit: usize,
    length: u64,
) -> io::Result<MessagePage> {
    // The hot path: a poller that is already up to date pays one `stat` and stops here.
    if start == length {
        return Ok(MessagePage {
            cursor: encode_cursor(start),
            start_cursor: encode_cursor(start),
            has_more: false,
            has_more_before: start > 0,
            messages: Vec::new(),
        });
    }
    if length - start > MAX_INCREMENTAL {
        return read_tail(path, agent, limit, length);
    }
    // A forward cursor always sits on a line boundary, so nothing is skipped at the leading
    // edge -- unlike a window, which lands wherever the arithmetic put it.
    let scan = scan(path, agent, start, false, None)?;
    Ok(page_forward(scan, limit, start))
}

/// Reads the page immediately before `boundary`, scanning a bounded window backwards rather
/// than parsing from byte 0.
fn read_before(
    path: &Path,
    agent: AgentKind,
    boundary: u64,
    limit: usize,
) -> io::Result<MessagePage> {
    if boundary == 0 {
        return Ok(MessagePage::empty());
    }
    let narrow = scan_window(path, agent, boundary, WINDOW)?;
    if !narrow.messages.is_empty() || narrow.start == 0 {
        return Ok(page_backward(narrow, limit, boundary));
    }
    let wide = scan_window(path, agent, boundary, WIDE_WINDOW)?;
    Ok(page_backward(wide, limit, boundary))
}

fn read_tail(path: &Path, agent: AgentKind, limit: usize, length: u64) -> io::Result<MessagePage> {
    let narrow = scan_window(path, agent, length, WINDOW)?;
    if !narrow.messages.is_empty() || narrow.start == 0 {
        return Ok(page_tail(narrow, limit));
    }
    let wide = scan_window(path, agent, length, WIDE_WINDOW)?;
    Ok(page_tail(wide, limit))
}

/// Scans the `window` bytes ending at `boundary`. The window starts wherever the subtraction
/// lands, which is usually the middle of a line, so the leading partial line is dropped.
fn scan_window(path: &Path, agent: AgentKind, boundary: u64, window: u64) -> io::Result<Scan> {
    let start = boundary.saturating_sub(window);
    scan(path, agent, start, start > 0, Some(boundary))
}

struct Scanned {
    message: Message,
    /// Offset of the first byte of the transcript line that produced this message.
    start: u64,
    /// Offset just past that line.
    end: u64,
}

struct Scan {
    messages: Vec<Scanned>,
    /// Where scanning began, *before* any partial leading line was dropped. Paging backwards
    /// falls back to this when a window yields nothing, which is what guarantees each "load
    /// earlier" moves strictly towards the start of the file.
    start: u64,
    /// Offset just past the last *complete* line. A transcript being written to can end in a
    /// partial line; consuming it would skip the record once it is finished.
    end: u64,
}

fn scan(
    path: &Path,
    agent: AgentKind,
    start: u64,
    skip_partial_line: bool,
    boundary: Option<u64>,
) -> io::Result<Scan> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let parse = parser_for(agent);
    let mut line = Vec::new();
    let mut offset = start;

    if skip_partial_line {
        offset += reader.read_until(b'\n', &mut line)? as u64;
    }

    let mut messages: Vec<Scanned> = Vec::new();
    while boundary.is_none_or(|boundary| offset < boundary) {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || !line.ends_with(b"\n") {
            break;
        }
        let line_start = offset;
        offset += read as u64;
        let raw = String::from_utf8_lossy(&line);
        let push = |messages: &mut Vec<Scanned>, message| {
            messages.push(Scanned {
                message,
                start: line_start,
                end: offset,
            });
        };
        match parse(raw.trim_end(), line_start) {
            LineOutcome::Ignore => {}
            LineOutcome::Emit(message) => push(&mut messages, message),
            // The condensed history duplicates lines the transcript already spelled out, so it
            // is only rendered when this window starts after those lines and holds nothing yet.
            LineOutcome::ReplacePriorHistory(history) => {
                if messages.is_empty() {
                    for message in history {
                        push(&mut messages, message);
                    }
                }
            }
        }
    }
    Ok(Scan {
        messages,
        start,
        end: offset,
    })
}

/// A forward read returns the *oldest* unseen messages, so a client that fell behind catches
/// up in order rather than jumping over the middle of the conversation.
fn page_forward(scan: Scan, limit: usize, requested: u64) -> MessagePage {
    let Scan { messages, end, .. } = scan;
    let has_more = messages.len() > limit;
    let cursor = if has_more {
        messages[limit - 1].end
    } else {
        end
    };
    let kept = messages.into_iter().take(limit).collect::<Vec<_>>();
    let start_cursor = kept.first().map_or(requested, |scanned| scanned.start);
    MessagePage {
        cursor: encode_cursor(cursor),
        start_cursor: encode_cursor(start_cursor),
        has_more,
        has_more_before: start_cursor > 0,
        messages: kept.into_iter().map(|scanned| scanned.message).collect(),
    }
}

/// A fresh read returns the *newest* messages: the bottom of the conversation is what the user
/// is looking at. The cursor still points past everything scanned, so the first poll after it
/// asks only for what arrives next.
fn page_tail(scan: Scan, limit: usize) -> MessagePage {
    let Scan {
        messages,
        start,
        end,
    } = scan;
    let (kept, start_cursor, has_more_before) = trim_to_newest(messages, limit, start);
    MessagePage {
        cursor: encode_cursor(end),
        start_cursor: encode_cursor(start_cursor),
        has_more: false,
        has_more_before,
        messages: kept,
    }
}

/// A backward read returns the newest messages of the window -- the ones adjacent to the page
/// the client already has -- still ordered oldest to newest so it can prepend them as-is.
fn page_backward(scan: Scan, limit: usize, boundary: u64) -> MessagePage {
    let Scan {
        messages, start, ..
    } = scan;
    let cursor = messages.last().map_or(boundary, |scanned| scanned.end);
    let (kept, start_cursor, has_more_before) = trim_to_newest(messages, limit, start);
    MessagePage {
        cursor: encode_cursor(cursor),
        start_cursor: encode_cursor(start_cursor),
        // Everything past this page is history the client asked to page back from, so there is
        // nothing forward for it to fetch.
        has_more: false,
        has_more_before,
        messages: kept,
    }
}

/// Keeps the newest `limit` messages of a window and reports where the kept run starts and
/// whether anything was left behind it.
fn trim_to_newest(
    messages: Vec<Scanned>,
    limit: usize,
    window_start: u64,
) -> (Vec<Message>, u64, bool) {
    let skip = messages.len().saturating_sub(limit);
    let has_more_before = skip > 0 || window_start > 0;
    let kept = messages.into_iter().skip(skip).collect::<Vec<_>>();
    let start_cursor = kept.first().map_or(window_start, |scanned| scanned.start);
    (
        kept.into_iter().map(|scanned| scanned.message).collect(),
        start_cursor,
        has_more_before,
    )
}

fn encode_cursor(offset: u64) -> String {
    format!("{CURSOR_PREFIX}{offset}")
}

fn decode_cursor(value: &str) -> Option<u64> {
    value.strip_prefix(CURSOR_PREFIX)?.parse().ok()
}

#[cfg(test)]
mod tests;
