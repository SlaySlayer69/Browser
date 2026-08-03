//! Small allocation-conscious helpers shared across modules.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix time in milliseconds. Used as the timestamp for every stored row so
/// the schema never needs a text date format.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Host part of a URL, lowercase, without a trailing dot.
pub fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase()))
}

/// FNV-1a. We use it for the blocker's decision cache keys, where we need a
/// cheap, dependency-free hash over a handful of short strings and collisions
/// are merely a cache miss risk, not a correctness risk (see `blocker::cache`).
#[inline]
pub fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Turn whatever the user typed in the omnibox into either a URL to navigate
/// to or a search query. Kept deliberately dumb: no network probing, no DNS.
pub fn omnibox_to_url(input: &str, search_template: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Explicit scheme wins outright.
    if let Ok(parsed) = url::Url::parse(trimmed) {
        if matches!(parsed.scheme(), "http" | "https" | "file" | "about" | "cleandark") {
            return trimmed.to_string();
        }
    }

    // "localhost", "localhost:3000", "example.com/path" -> https://
    let looks_like_host = !trimmed.contains(char::is_whitespace)
        && (trimmed.starts_with("localhost")
            || trimmed
                .split('/')
                .next()
                .map(|authority| {
                    let host = authority.split(':').next().unwrap_or(authority);
                    host.contains('.') && !host.ends_with('.') && !host.starts_with('.')
                })
                .unwrap_or(false));

    if looks_like_host {
        return format!("https://{trimmed}");
    }

    search_template.replace("{q}", &urlencode(trimmed))
}

/// Percent-encode a query component. `url::form_urlencoded` pulls in a
/// serializer we do not otherwise need, so this stays hand-rolled and small.
pub fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => {
                out.push('%');
                out.push_str(&format!("{other:02X}"));
            }
        }
    }
    out
}

/// Shorten a URL for display in the tab strip / history list without
/// allocating when it is already short enough.
pub fn elide(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = "https://duckduckgo.com/?q={q}";

    #[test]
    fn explicit_urls_pass_through() {
        assert_eq!(omnibox_to_url("https://example.com/a", SEARCH), "https://example.com/a");
        assert_eq!(omnibox_to_url("about:blank", SEARCH), "about:blank");
    }

    #[test]
    fn bare_hosts_get_https() {
        assert_eq!(omnibox_to_url("example.com", SEARCH), "https://example.com");
        assert_eq!(omnibox_to_url("example.com/x?y=1", SEARCH), "https://example.com/x?y=1");
        assert_eq!(omnibox_to_url("localhost:3000", SEARCH), "https://localhost:3000");
    }

    #[test]
    fn prose_becomes_a_search() {
        assert_eq!(
            omnibox_to_url("rust webview2 memory", SEARCH),
            "https://duckduckgo.com/?q=rust+webview2+memory"
        );
        // A single word with no dot is a search, not a host.
        assert_eq!(omnibox_to_url("rust", SEARCH), "https://duckduckgo.com/?q=rust");
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://Example.COM/x").as_deref(), Some("example.com"));
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn elide_keeps_short_strings() {
        assert_eq!(elide("abc", 5), "abc");
        assert_eq!(elide("abcdefg", 4), "abc\u{2026}");
    }
}
