//! Shell command-prefix matching for the allowlist.

use crate::policy::CommandPrefix;

/// Whether every clause of `command` is covered by an allowlist prefix.
///
/// The command is split on clause separators (`;`, `|`, `&`, newlines)
/// respecting quotes; every resulting clause must start with one of the
/// prefixes token-wise. Command substitution and subshells are rejected
/// outright, so an allowlist entry cannot be abused to smuggle a second
/// command past the check.
///
/// An empty allowlist allows nothing, and so does an empty command.
#[must_use]
pub fn is_allowed(
    command: &str,
    allow: &[CommandPrefix],
) -> bool {
    let Some(clauses) = split_clauses(command) else {
        return false;
    };
    !clauses.is_empty() && clauses.iter().all(|clause| clause_matches(clause, allow))
}

/// Splits a command into clauses, or `None` when the command uses shell
/// constructs the allowlist cannot reason about: command substitution,
/// subshells, or unterminated quotes.
fn split_clauses(command: &str) -> Option<Vec<String>> {
    let mut clauses = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();

    while let Some(char) = chars.next() {
        match char {
            // Quote characters are preserved: the clause is handed back to
            // the shell tokenizer, which must see the original quoting.
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(char);
            },
            '"' if !in_single => {
                in_double = !in_double;
                current.push(char);
            },
            '\\' => {
                current.push(char);
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            },
            '`' if !in_single => return None,
            '$' if !in_single => {
                current.push(char);
                if chars.peek() == Some(&'(') {
                    return None;
                }
            },
            '(' | ')' if !in_single && !in_double => return None,
            ';' | '\n' if !in_single && !in_double => push_clause(&mut clauses, &mut current),
            '|' | '&' if !in_single && !in_double => {
                if chars.peek() == Some(&char) {
                    chars.next();
                }
                push_clause(&mut clauses, &mut current);
            },
            _ => current.push(char),
        }
    }
    if in_single || in_double {
        return None;
    }
    push_clause(&mut clauses, &mut current);
    Some(clauses)
}

fn push_clause(
    clauses: &mut Vec<String>,
    current: &mut String,
) {
    let clause = current.trim().to_string();
    current.clear();
    if !clause.is_empty() {
        clauses.push(clause);
    }
}

fn clause_matches(
    clause: &str,
    allow: &[CommandPrefix],
) -> bool {
    let Some(tokens) = shlex::split(clause) else {
        return false;
    };
    allow.iter().any(|prefix| {
        let Some(prefix_tokens) = shlex::split(prefix.as_str()) else {
            return false;
        };
        // An empty prefix tokenizes to no tokens; treating it as matching would
        // make it an allow-everything entry, so it never matches. `Policy::validate`
        // rejects such a prefix at construction as well.
        !prefix_tokens.is_empty()
            && prefix_tokens.len() <= tokens.len()
            && prefix_tokens
                .iter()
                .zip(&tokens)
                .all(|(prefix_token, token)| prefix_token == token)
    })
}

#[cfg(test)]
mod tests {
    use super::is_allowed;
    use crate::policy::CommandPrefix;

    fn allow(entries: &[&str]) -> Vec<CommandPrefix> {
        entries
            .iter()
            .map(|entry| CommandPrefix::new(*entry))
            .collect()
    }

    #[test]
    fn empty_allowlist_allows_nothing() {
        assert!(!is_allowed("ls", &[]));
        assert!(!is_allowed("", &[]));
    }

    #[test]
    fn prefixes_match_token_wise() {
        let allow = allow(&["cargo test", "git status", "ls"]);

        assert!(is_allowed("cargo test --workspace", &allow));
        assert!(is_allowed("git status --porcelain", &allow));
        assert!(is_allowed("ls -la", &allow));
        assert!(!is_allowed("cargo build", &allow));
        assert!(!is_allowed("git push", &allow));
        assert!(
            !is_allowed("cargo", &allow),
            "a longer prefix than the command"
        );
        assert!(
            !is_allowed("cargot test", &allow),
            "no partial token matches"
        );
    }

    #[test]
    fn compound_commands_need_every_clause_allowed() {
        let allow = allow(&["cargo test", "head"]);

        assert!(is_allowed("cargo test | head", &allow));
        assert!(is_allowed("cargo test && cargo test", &allow));
        assert!(!is_allowed("cargo test; curl example.com", &allow));
        assert!(!is_allowed("cargo test || curl example.com", &allow));
        assert!(!is_allowed("cargo test & curl example.com", &allow));
        assert!(!is_allowed("cargo test\ncurl example.com", &allow));
    }

    #[test]
    fn quoted_operators_do_not_split() {
        let allow = allow(&["echo"]);

        assert!(is_allowed("echo 'a;b|c'", &allow));
        assert!(is_allowed("echo \"a && b\"", &allow));
        assert!(is_allowed("echo \"it's fine\"", &allow));
    }

    #[test]
    fn command_substitution_and_subshells_are_rejected() {
        let allow = allow(&["echo", "cargo test"]);

        assert!(!is_allowed("echo $(curl example.com)", &allow));
        assert!(!is_allowed("echo `curl example.com`", &allow));
        assert!(!is_allowed("$(curl example.com)", &allow));
        assert!(!is_allowed("echo (whoami)", &allow));
        assert!(!is_allowed("echo 'unterminated", &allow));
        assert!(!is_allowed("echo \"unterminated", &allow));
    }

    #[test]
    fn empty_commands_are_denied_and_trailing_separators_are_inert() {
        let allow = allow(&["cargo test"]);

        assert!(!is_allowed("   ", &allow));
        assert!(!is_allowed(";", &allow));
        assert!(is_allowed("cargo test;", &allow));
    }

    #[test]
    fn empty_or_whitespace_prefixes_match_nothing() {
        let allow = allow(&["", "   "]);

        assert!(!is_allowed("curl example.com", &allow));
        assert!(!is_allowed("rm -rf /", &allow));
        assert!(!is_allowed("cargo test", &allow));
    }
}
