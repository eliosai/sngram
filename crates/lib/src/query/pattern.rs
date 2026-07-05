//! Lightweight pattern facts used outside HIR analysis.

use regex_syntax::ast::{self, Ast};

/// Facts read from the pattern's AST before HIR planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternFacts {
    inline_case_insensitive: bool,
}

impl PatternFacts {
    /// Analyze one pattern. Invalid syntax returns no facts; the parser reports
    /// the concrete error afterward.
    #[must_use]
    pub fn analyze(pattern: &str) -> Self {
        let Ok(parsed) = ast::parse::Parser::new().parse(pattern) else {
            return Self {
                inline_case_insensitive: false,
            };
        };
        Self {
            inline_case_insensitive: InlineFlagAnalysis::visit(&parsed),
        }
    }

    /// Whether the regex enables case-insensitive matching through inline
    /// flags. A scoped `(?i:...)` only affects part of the regex, but using the
    /// folded gram space for the whole plan remains sound.
    #[must_use]
    pub const fn uses_folded_space(self) -> bool {
        self.inline_case_insensitive
    }
}

struct InlineFlagAnalysis;

impl InlineFlagAnalysis {
    fn visit(node: &Ast) -> bool {
        match *node {
            Ast::Empty(_)
            | Ast::Literal(_)
            | Ast::Dot(_)
            | Ast::Assertion(_)
            | Ast::ClassUnicode(_)
            | Ast::ClassPerl(_)
            | Ast::ClassBracketed(_) => false,
            Ast::Flags(ref flags) => Self::flags(&flags.flags),
            Ast::Repetition(ref repetition) => Self::visit(&repetition.ast),
            Ast::Group(ref group) => {
                group.flags().is_some_and(Self::flags) || Self::visit(&group.ast)
            },
            Ast::Alternation(ref alternation) => alternation.asts.iter().any(Self::visit),
            Ast::Concat(ref concat) => concat.asts.iter().any(Self::visit),
        }
    }

    fn flags(flags: &ast::Flags) -> bool {
        matches!(flags.flag_state(ast::Flag::CaseInsensitive), Some(true))
    }
}

#[cfg(test)]
mod tests {
    use super::PatternFacts;

    #[test]
    fn detects_inline_case_insensitive_flags() {
        assert!(PatternFacts::analyze("(?i)foo").uses_folded_space());
        assert!(PatternFacts::analyze("foo(?i:bar)baz").uses_folded_space());
        assert!(PatternFacts::analyze("(?:foo|(?i:bar))").uses_folded_space());
        assert!(!PatternFacts::analyze("foo").uses_folded_space());
        assert!(!PatternFacts::analyze("(?-i:foo)").uses_folded_space());
        assert!(!PatternFacts::analyze("foo(").uses_folded_space());
    }
}
