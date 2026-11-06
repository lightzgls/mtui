//! Shared YouTube Music client identity and renderer field parsing.

use std::time::Duration;

pub(in crate::source) const MUSIC_CLIENT_NAME: &str = "WEB_REMIX";
pub(in crate::source) const MUSIC_CLIENT_VERSION: &str = "1.20241127.01.00";

/// Joins the text runs of one display column into a single string.
///
/// Shared with [`crate::source::catalog::home`]: the flexible-column shape is the same
/// wherever YouTube Music draws a list row, whatever the row happens to mean.
pub(in crate::source) fn flex_column(item: &serde_json::Value, index: usize) -> Option<String> {
    let runs = flex_runs(item, index)?.as_array()?;

    let text: String = runs.iter().filter_map(|run| run["text"].as_str()).collect();
    (!text.is_empty()).then_some(text)
}

/// Navigation-bearing runs behind one flexible display column.
pub(in crate::source) fn flex_runs(item: &serde_json::Value, index: usize) -> Option<&serde_json::Value> {
    item.pointer(&format!(
        "/flexColumns/{index}/musicResponsiveListItemFlexColumnRenderer/text/runs"
    ))
}

/// Parses `M:SS` or `H:MM:SS` into a duration. Anything else is `None`, which
/// the UI renders as `LIVE`.
pub(in crate::source) fn parse_duration(text: &str) -> Option<Duration> {
    let mut secs: u64 = 0;
    let mut fields = 0;
    for field in text.trim().split(':') {
        secs = secs
            .checked_mul(60)?
            .checked_add(field.trim().parse().ok()?)?;
        fields += 1;
    }
    (2..=3).contains(&fields).then(|| Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("3:47"), Some(Duration::from_secs(227)));
        assert_eq!(parse_duration(" 9:05 "), Some(Duration::from_secs(545)));
        assert_eq!(parse_duration("1:02:05"), Some(Duration::from_secs(3725)));
    }

    #[test]
    fn rejects_things_that_are_not_durations() {
        // A bare number is a play count or a year, not a length.
        assert_eq!(parse_duration("2013"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("Random Access Memories"), None);
        assert_eq!(parse_duration("1:2:3:4"), None);
    }
}
