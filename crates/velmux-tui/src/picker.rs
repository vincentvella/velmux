//! The location picker: every place you can work (repo bases, claimed slots,
//! unclassified folders), filtered by a fuzzy query as you type.

use crate::pane::Loc;

#[derive(Debug, Clone)]
pub struct Item {
    pub loc: Loc,
    /// What's matched and shown, e.g. `caliburr › slot-2 · feat/login`.
    pub label: String,
}

#[derive(Debug, Default)]
pub struct Picker {
    pub query: String,
    /// Index into the filtered list.
    pub selected: usize,
}

/// Fuzzy subsequence score (higher is better), `None` if `query` doesn't match.
/// Rewards consecutive runs and matches at word starts; case-insensitive.
pub fn score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut pos = 0;
    let mut prev: Option<usize> = None;
    for q in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (pos..text.len()).find(|&i| text[i] == q)?;
        score += 1;
        if prev == Some(found.wrapping_sub(1)) {
            score += 5; // consecutive
        }
        let boundary = found == 0 || matches!(text[found - 1], ' ' | '/' | '-' | '_' | '›' | '·' | '.');
        if boundary {
            score += 3;
        }
        if prev.is_some() {
            score -= (found - pos) as i32; // gaps between matched chars cost
        }
        prev = Some(found);
        pos = found + 1;
    }
    Some(score)
}

/// Items matching `query`, best first (ties keep their original order).
pub fn filter<'a>(items: &'a [Item], query: &str) -> Vec<&'a Item> {
    let mut hits: Vec<(i32, usize, &Item)> =
        items.iter().enumerate().filter_map(|(i, it)| score(query, &it.label).map(|s| (s, i, it))).collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, _, it)| it).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(labels: &[&str]) -> Vec<Item> {
        labels
            .iter()
            .enumerate()
            .map(|(i, l)| Item { loc: Loc { project: i, slot: None }, label: l.to_string() })
            .collect()
    }

    #[test]
    fn scores_subsequences_and_prefers_tight_word_matches() {
        assert!(score("cal", "caliburr › base").is_some());
        assert!(score("cb", "caliburr › base").is_some());
        assert_eq!(score("xyz", "caliburr › base"), None);
        assert_eq!(score("", "anything"), Some(0));
        // A tight word-start match beats a scattered one.
        assert!(score("web", "app › slot-1 · feat/web").unwrap() > score("web", "w-e-b").unwrap());
        assert!(score("BASE", "kino › base").is_some(), "case-insensitive");
    }

    #[test]
    fn filters_best_first_keeping_order_on_ties() {
        let it = items(&["devloop › base", "caliburr › base", "caliburr › slot-1 · feat/sentry", "kino › base"]);
        let got: Vec<&str> = filter(&it, "").iter().map(|i| i.label.as_str()).collect();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], "devloop › base");
        let got: Vec<&str> = filter(&it, "sentry").iter().map(|i| i.label.as_str()).collect();
        assert_eq!(got, ["caliburr › slot-1 · feat/sentry"]);
        let got: Vec<&str> = filter(&it, "cali").iter().map(|i| i.label.as_str()).collect();
        assert_eq!(got, ["caliburr › base", "caliburr › slot-1 · feat/sentry"]);
    }
}
