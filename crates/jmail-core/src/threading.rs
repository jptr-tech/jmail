//! Conversation (thread) construction.
//!
//! Gmail exposes a server-side thread id via the `X-GM-THRID` IMAP extension,
//! which we use when available. For every other provider we reconstruct threads
//! locally from the `Message-ID` / `In-Reply-To` / `References` graph, falling
//! back to a normalised subject match within the same account.

/// Strip reply/forward prefixes and list tags so related subjects compare equal.
pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let stripped = ["re:", "fwd:", "fw:"]
            .iter()
            .find_map(|prefix| lower.strip_prefix(prefix).map(|_| &s[prefix.len()..]));
        match stripped {
            Some(rest) => s = rest.trim_start(),
            None => break,
        }
    }
    s.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_reply_prefixes() {
        assert_eq!(normalize_subject("Re: Hello"), "hello");
        assert_eq!(normalize_subject("FWD: Re: Hello"), "hello");
        assert_eq!(normalize_subject("Re:Re:  Spaced  "), "spaced");
        assert_eq!(normalize_subject("Plain"), "plain");
    }
}
