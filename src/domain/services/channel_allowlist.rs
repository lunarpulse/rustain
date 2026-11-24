//! Channel allowlist grammar shared by `rustain init` and the daemon (Story C-12).
//!
//! One policy, two consumers: the init wizard parses what the operator types and
//! the Slack composition re-validates what the profile stores, so a value the
//! wizard accepts is exactly a value the daemon accepts. Nothing here echoes a
//! rejected value: errors carry the 1-based entry index and a kind, never the
//! text (an operator may paste a token by mistake).

/// Why one allowlist entry was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowlistEntryError {
    /// Nothing between two commas (or a trailing comma).
    Empty,
    /// A wildcard such as `*`.
    Wildcard,
    /// A Slack conversation (`C`/`G`/`D`) ID where a sender user ID is required.
    ConversationId,
    /// Not a sender user ID of the channel's kind (a name, a handle, junk).
    NotAnId,
    /// A Telegram chat ID that is negative (group/channel) or out of range.
    OutOfRange,
}

/// A rejected list: the 1-based index of the first bad entry and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllowlistError {
    pub index: usize,
    pub kind: AllowlistEntryError,
}

/// Longest Slack ID accepted: far above real IDs (9–11), still bounded.
const SLACK_ID_MAX_LEN: usize = 32;

/// Classify one trimmed Slack entry as a sender user ID.
///
/// Slack user IDs start with `U` (workspace members) or `W` (Enterprise Grid)
/// followed by uppercase ASCII letters and digits. Case is never folded: a
/// lowercase look-alike is rejected, not corrected, because the event's `user`
/// field is compared byte for byte.
pub fn classify_slack_user_id(entry: &str) -> Result<(), AllowlistEntryError> {
    if entry.is_empty() {
        return Err(AllowlistEntryError::Empty);
    }
    if entry.contains('*') {
        return Err(AllowlistEntryError::Wildcard);
    }
    let mut chars = entry.chars();
    let first = chars.next().unwrap_or(' ');
    let body_ok = entry.len() >= 2
        && entry.len() <= SLACK_ID_MAX_LEN
        && entry
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    match first {
        'U' | 'W' if body_ok => Ok(()),
        'C' | 'G' | 'D' if body_ok => Err(AllowlistEntryError::ConversationId),
        _ => Err(AllowlistEntryError::NotAnId),
    }
}

/// `true` when `entry` is a valid Slack sender user ID.
pub fn is_slack_user_id(entry: &str) -> bool {
    classify_slack_user_id(entry).is_ok()
}

/// Parse a comma-separated Slack sender allowlist: trim each entry, reject empty
/// items, wildcards, conversation IDs and anything else that is not a user ID,
/// and deduplicate in first-seen order. An all-blank input is not an error here;
/// callers treat an empty result as "go back".
pub fn parse_slack_user_ids(input: &str) -> Result<Vec<String>, AllowlistError> {
    parse_list(input, |entry| {
        classify_slack_user_id(entry).map(|()| entry.to_string())
    })
}

/// Parse a comma-separated Telegram chat allowlist into positive decimal `i64`s.
/// Usernames, signed group IDs, `+`-prefixed and non-decimal forms are rejected.
pub fn parse_telegram_chat_ids(input: &str) -> Result<Vec<i64>, AllowlistError> {
    parse_list(input, |entry| {
        if entry.is_empty() {
            return Err(AllowlistEntryError::Empty);
        }
        if entry.contains('*') {
            return Err(AllowlistEntryError::Wildcard);
        }
        if entry.starts_with('-') {
            return Err(AllowlistEntryError::OutOfRange);
        }
        if !entry.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AllowlistEntryError::NotAnId);
        }
        match entry.parse::<i64>() {
            Ok(id) if id > 0 => Ok(id),
            _ => Err(AllowlistEntryError::OutOfRange),
        }
    })
}

fn parse_list<T: PartialEq>(
    input: &str,
    mut parse: impl FnMut(&str) -> Result<T, AllowlistEntryError>,
) -> Result<Vec<T>, AllowlistError> {
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<T> = Vec::new();
    for (position, raw) in input.split(',').enumerate() {
        let value = parse(raw.trim()).map_err(|kind| AllowlistError {
            index: position + 1,
            kind,
        })?;
        if !out.contains(&value) {
            out.push(value);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_ids_are_trimmed_deduplicated_in_first_seen_order_and_case_preserved() {
        let ids = parse_slack_user_ids(" U012ABCDEF , W0345GH ,U012ABCDEF,UZ9 ").unwrap();
        assert_eq!(ids, ["U012ABCDEF", "W0345GH", "UZ9"]);
    }

    #[test]
    fn slack_rejections_name_the_index_and_kind_only() {
        let kinds = |input| parse_slack_user_ids(input).unwrap_err();
        assert_eq!(
            kinds("U012ABCDEF, C012ABCDEF"),
            AllowlistError {
                index: 2,
                kind: AllowlistEntryError::ConversationId
            }
        );
        assert_eq!(kinds("U01,,U02").kind, AllowlistEntryError::Empty);
        assert_eq!(kinds("U01,").index, 2);
        assert_eq!(kinds("*").kind, AllowlistEntryError::Wildcard);
        assert_eq!(kinds("U*").kind, AllowlistEntryError::Wildcard);
        assert_eq!(kinds("@dana").kind, AllowlistEntryError::NotAnId);
        assert_eq!(kinds("u012abcdef").kind, AllowlistEntryError::NotAnId);
        assert_eq!(kinds("Dana Lee").kind, AllowlistEntryError::NotAnId);
        assert_eq!(kinds("U").kind, AllowlistEntryError::NotAnId);
        let long = format!("U{}", "A".repeat(SLACK_ID_MAX_LEN));
        assert_eq!(kinds(&long).kind, AllowlistEntryError::NotAnId);
        // DM conversation IDs are conversation IDs too.
        assert_eq!(kinds("D01234").kind, AllowlistEntryError::ConversationId);
    }

    #[test]
    fn blank_input_is_an_empty_list_not_an_error() {
        assert_eq!(parse_slack_user_ids("  ").unwrap(), Vec::<String>::new());
        assert_eq!(parse_telegram_chat_ids("").unwrap(), Vec::<i64>::new());
    }

    #[test]
    fn telegram_ids_are_positive_decimal_i64() {
        assert_eq!(
            parse_telegram_chat_ids("42, 7 ,42,9223372036854775807").unwrap(),
            [42, 7, i64::MAX]
        );
        for (input, kind) in [
            ("-100123", AllowlistEntryError::OutOfRange),
            ("0", AllowlistEntryError::OutOfRange),
            ("9223372036854775808", AllowlistEntryError::OutOfRange),
            ("+5", AllowlistEntryError::NotAnId),
            ("0x10", AllowlistEntryError::NotAnId),
            ("@dana", AllowlistEntryError::NotAnId),
            ("1,,2", AllowlistEntryError::Empty),
            ("*", AllowlistEntryError::Wildcard),
        ] {
            assert_eq!(
                parse_telegram_chat_ids(input).unwrap_err().kind,
                kind,
                "{input}"
            );
        }
    }
}
