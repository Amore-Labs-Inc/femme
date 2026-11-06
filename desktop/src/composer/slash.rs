//! The `/` menu: which of the daemon's commands (`Req::Commands`) to offer while the draft is a
//! command word being typed. Pure, so the view in `composer.rs` only draws and routes keys.

/// The most rows the menu shows.
pub const MAX: usize = 8;

/// What follows the `/` while the user is still typing the command word: the text starts with `/`
/// and holds no whitespace yet. `None` for anything else (a prompt, a command with arguments).
pub fn query(text: &str) -> Option<&str> {
    text.strip_prefix('/')
        .filter(|q| !q.contains(char::is_whitespace))
}

/// The names to offer for `text`, at most [`MAX`]: those that start with the query (case aside)
/// alphabetically, then those that hold it elsewhere. An empty query offers the first names.
pub fn matches<'a>(text: &str, names: &'a [String]) -> Vec<&'a str> {
    let Some(q) = query(text) else {
        return Vec::new();
    };
    let q = q.to_lowercase();
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let (mut prefix, mut inside) = (Vec::new(), Vec::new());
    for name in sorted {
        let low = name.to_lowercase();
        if low.starts_with(&q) {
            prefix.push(name);
        } else if low.contains(&q) {
            inside.push(name);
        }
    }
    prefix.extend(inside);
    prefix.truncate(MAX);
    prefix
}

/// Whether `text` is exactly `/` and one of the names: Enter then sends it rather than completing.
pub fn exact(text: &str, names: &[String]) -> bool {
    text.strip_prefix('/')
        .is_some_and(|q| names.iter().any(|n| n == q))
}

/// The input's text once `name` is picked: the command and a space for its arguments.
pub fn complete(name: &str) -> String {
    format!("/{name} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn only_a_command_word_being_typed_is_a_query() {
        assert_eq!(query("/"), Some(""));
        assert_eq!(query("/rev"), Some("rev"));
        assert_eq!(query("/review "), None);
        assert_eq!(query("/review\n"), None);
        assert_eq!(query("hi /rev"), None);
        assert_eq!(query(""), None);
    }

    #[test]
    fn prefix_matches_come_first_then_the_rest() {
        let n = names(&["context", "compact", "btw", "review", "pr-comments", "cost"]);
        assert_eq!(
            matches("/co", &n),
            ["compact", "context", "cost", "pr-comments"]
        );
        assert_eq!(
            matches("/CO", &n),
            ["compact", "context", "cost", "pr-comments"]
        );
        assert_eq!(matches("/view", &n), ["review"]);
        assert!(matches("/zzz", &n).is_empty());
        assert!(matches("/co x", &n).is_empty());
        assert!(matches("co", &n).is_empty());
    }

    #[test]
    fn an_empty_query_offers_the_first_eight_alphabetically() {
        let n = names(&["j", "i", "h", "g", "f", "e", "d", "c", "b", "a", "a"]);
        assert_eq!(matches("/", &n), ["a", "b", "c", "d", "e", "f", "g", "h"]);
        assert!(matches("/", &[]).is_empty());
    }

    #[test]
    fn exact_names_a_command_and_nothing_more() {
        let n = names(&["review", "compact"]);
        assert!(exact("/review", &n));
        assert!(!exact("/rev", &n));
        assert!(!exact("/review ", &n));
        assert!(!exact("review", &n));
        assert_eq!(complete("review"), "/review ");
    }
}
