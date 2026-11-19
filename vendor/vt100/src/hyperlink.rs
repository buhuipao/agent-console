//! OSC 8 web links shared by the pen and the cells it writes.
use std::sync::Arc;

// A thin pointer keeps the per-cell cost to one word.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Hyperlink(Option<Arc<String>>);

impl Hyperlink {
    pub(crate) fn from_uri(parts: &[&[u8]]) -> Self {
        let size = parts.iter().map(|p| p.len()).sum::<usize>()
            + parts.len().saturating_sub(1);
        // vte exposes at most 16 OSC fields, including the command and params.
        // At that limit the last URI field may have been truncated.
        if size == 0 || size > 8192 || parts.len() >= 14 {
            return Self::default();
        }
        let Ok(uri) = String::from_utf8(parts.join(&b';')) else {
            return Self::default();
        };
        if !is_web_url(&uri) {
            return Self::default();
        }
        Self(Some(Arc::new(uri)))
    }

    pub(crate) fn uri(&self) -> Option<&str> {
        self.0.as_ref().map(|uri| uri.as_str())
    }

    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b]8;;");
        if let Some(uri) = &self.0 {
            out.extend_from_slice(uri.as_bytes());
        }
        out.extend_from_slice(b"\x1b\\");
    }

    pub(crate) fn transition(&self, out: &mut Vec<u8>, previous: &Self) {
        if self != previous {
            self.write(out);
        }
    }
}

/// Whether a terminal link may be handed to the desktop browser.
#[must_use]
pub fn is_web_url(uri: &str) -> bool {
    uri.split_once("://").is_some_and(|(scheme, target)| {
        (scheme.eq_ignore_ascii_case("https")
            || scheme.eq_ignore_ascii_case("http"))
            && !target.is_empty()
            && uri.len() <= 8192
            && !uri.chars().any(|c| c.is_control() || c.is_whitespace())
    })
}
