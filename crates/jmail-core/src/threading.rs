//! Conversation (thread) construction.
//!
//! Gmail exposes a server-side thread id via the `X-GM-THRID` IMAP extension,
//! which we use when available. For every other provider we reconstruct threads
//! locally from the `Message-ID` / `In-Reply-To` / `References` graph, falling
//! back to a normalised subject match within the same account.

use std::collections::HashMap;

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

/// Normalise a `Message-ID` header value: strip surrounding angle brackets,
/// whitespace and any leading/trailing noise such as `<>` in a References list.
pub fn normalize_message_id(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.trim_start_matches('<');
    // The id may be followed by another `<...>` in a References list; keep only
    // up to the closing angle bracket.
    let raw = raw.split('>').next().unwrap_or(raw);
    raw.trim().trim_matches('<').trim().to_string()
}

/// The threading-relevant headers of one message.
#[derive(Debug, Clone)]
pub struct ThreadInput {
    pub id: i64,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub subject: String,
}

/// Assign a conversation key to every message.
///
/// Messages are linked when one's `In-Reply-To`/`References` names the other's
/// `Message-ID`. Messages with no resolvable link fall back to a normalised
/// subject match. Each connected component becomes one thread, keyed by the
/// `Message-ID` of its earliest (lowest-id) member, or `local-<id>` when that
/// member has no `Message-ID`.
pub fn assign_threads(messages: &[ThreadInput]) -> HashMap<i64, String> {
    let mut parent: Vec<usize> = (0..messages.len()).collect();

    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            // Keep the smaller root so the canonical thread owner is stable.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            parent[hi] = lo;
        }
    }

    // Message-ID -> first index that declares it.
    let mut by_message_id: HashMap<String, usize> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        let id = normalize_message_id(&message.message_id);
        if !id.is_empty() {
            by_message_id.entry(id).or_insert(index);
        }
    }

    // Link each message to any of its parents that we already have.
    let mut linked = vec![false; messages.len()];
    for (index, message) in messages.iter().enumerate() {
        let mut candidates: Vec<&str> = Vec::new();
        if !message.in_reply_to.is_empty() {
            candidates.push(&message.in_reply_to);
        }
        candidates.extend(message.references.iter().map(String::as_str));
        for candidate in candidates {
            let parent_id = normalize_message_id(candidate);
            if parent_id.is_empty() {
                continue;
            }
            if let Some(&parent_index) = by_message_id.get(&parent_id) {
                if parent_index != index {
                    union(&mut parent, index, parent_index);
                    linked[index] = true;
                }
            }
        }
    }

    // Subject fallback for messages with no resolvable parent.
    let mut by_subject: HashMap<String, usize> = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if linked[index] {
            continue;
        }
        let subject = normalize_subject(&message.subject);
        if subject.is_empty() {
            continue;
        }
        match by_subject.get(&subject) {
            Some(&first) => union(&mut parent, index, first),
            None => {
                by_subject.insert(subject, index);
            }
        }
    }

    // Canonical owner per component: the message that itself has no parent in
    // the component (the start of the reply chain), then the lowest message id
    // so the choice is deterministic. `linked` marks messages that resolved a
    // parent, so `!linked` are the roots.
    let mut owner_of: HashMap<usize, usize> = HashMap::new();
    for index in 0..messages.len() {
        let root = find(&mut parent, index);
        let is_root = !linked[index];
        match owner_of.get(&root).copied() {
            None => {
                owner_of.insert(root, index);
            }
            Some(current) => {
                let replace = match (!linked[current], is_root) {
                    (false, true) => true,
                    (true, false) => false,
                    _ => messages[index].id < messages[current].id,
                };
                if replace {
                    owner_of.insert(root, index);
                }
            }
        }
    }

    let mut assignment = HashMap::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let root = find(&mut parent, index);
        let owner = &messages[owner_of[&root]];
        let key = {
            let id = normalize_message_id(&owner.message_id);
            if id.is_empty() {
                format!("local-{}", owner.id)
            } else {
                id
            }
        };
        assignment.insert(message.id, key);
    }
    assignment
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(
        id: i64,
        message_id: &str,
        in_reply_to: &str,
        references: &[&str],
        subject: &str,
    ) -> ThreadInput {
        ThreadInput {
            id,
            message_id: message_id.to_string(),
            in_reply_to: in_reply_to.to_string(),
            references: references.iter().map(|s| s.to_string()).collect(),
            subject: subject.to_string(),
        }
    }

    #[test]
    fn strips_reply_prefixes() {
        assert_eq!(normalize_subject("Re: Hello"), "hello");
        assert_eq!(normalize_subject("FWD: Re: Hello"), "hello");
        assert_eq!(normalize_subject("Re:Re:  Spaced  "), "spaced");
        assert_eq!(normalize_subject("Plain"), "plain");
    }

    #[test]
    fn normalizes_message_ids() {
        assert_eq!(normalize_message_id("<a@b>"), "a@b");
        assert_eq!(normalize_message_id("  <a@b>  "), "a@b");
        assert_eq!(normalize_message_id("<a@b> <c@d>"), "a@b");
        assert_eq!(normalize_message_id("a@b"), "a@b");
    }

    #[test]
    fn groups_a_reply_chain() {
        let messages = [
            message(1, "root@x", "", &[], "Hello"),
            message(2, "reply@x", "<root@x>", &["<root@x>"], "Re: Hello"),
            message(3, "deep@x", "<reply@x>", &["<root@x>", "<reply@x>"], "Re: Hello"),
        ];
        let threads = assign_threads(&messages);
        assert_eq!(threads[&1], "root@x");
        assert_eq!(threads[&2], "root@x");
        assert_eq!(threads[&3], "root@x");
    }

    #[test]
    fn links_out_of_order_replies() {
        // The reply is cached before its parent.
        let messages = [
            message(1, "reply@x", "<parent@x>", &["<parent@x>"], "Re: Hello"),
            message(2, "parent@x", "", &[], "Hello"),
        ];
        let threads = assign_threads(&messages);
        assert_eq!(threads[&1], "parent@x");
        assert_eq!(threads[&2], "parent@x");
    }

    #[test]
    fn merges_branches_that_share_a_parent() {
        let messages = [
            message(1, "root@x", "", &[], "Hello"),
            message(2, "a@x", "<root@x>", &["<root@x>"], "Re: Hello"),
            message(3, "b@x", "<a@x>", &["<a@x>"], "Re: Hello"),
            message(4, "c@x", "<b@x>", &["<b@x>"], "Re: Hello"),
        ];
        let threads = assign_threads(&messages);
        assert!(threads.values().all(|t| t == "root@x"));
    }

    #[test]
    fn falls_back_to_subject_for_orphans() {
        let messages = [
            message(1, "a@x", "", &[], "Re: Quarterly report"),
            message(2, "b@x", "", &[], "Quarterly report"),
            message(3, "c@x", "", &[], "Something else"),
        ];
        let threads = assign_threads(&messages);
        assert_eq!(threads[&1], threads[&2]);
        assert_ne!(threads[&1], threads[&3]);
    }

    #[test]
    fn keeps_unrelated_subjects_separate() {
        let messages = [
            message(1, "a@x", "", &[], "Hello"),
            message(2, "b@x", "", &[], "Goodbye"),
        ];
        let threads = assign_threads(&messages);
        assert_ne!(threads[&1], threads[&2]);
    }
}
