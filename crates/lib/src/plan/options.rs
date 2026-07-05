//! Planner option helpers mirroring the verifier's matcher configuration.
//!
//! The public option types live in `crate::types`; this module keeps parser
//! analyses that interpret those options.

use regex_syntax::ast::{self, Ast, ClassSet, ClassSetItem};

/// Whether smart case resolves to case-insensitive for `pattern`: true iff
/// the pattern has at least one literal character and no uppercase literal.
///
/// This is `grep-regex`'s `AstAnalysis` rule over the parsed AST: escape
/// classes (`\W`, `\pL`), assertions (`\A`, `\b`), ASCII class names, and
/// group names are not literals, while class-set literals and range
/// endpoints are, after escape decoding (`\x41` is an uppercase `A`).
/// An unparseable pattern resolves to sensitive; the plan parse that
/// follows reports the error.
pub(super) fn smart_case_insensitive(pattern: &str) -> bool {
    let Ok(parsed) = ast::parse::Parser::new().parse(pattern) else {
        return false;
    };
    let mut analysis = CaseAnalysis::default();
    analysis.visit(&parsed);
    analysis.any_literal && !analysis.any_uppercase
}

/// Whether the regex enables case-insensitive matching through inline flags.
///
/// A scoped `(?i:...)` only affects part of the regex, but picking the folded
/// gram space for the whole plan is still sound: exact verification keeps the
/// original case-sensitive semantics, while the prefilter avoids HIR variant
/// explosion for the insensitive region.
pub(super) fn has_inline_case_insensitive(pattern: &str) -> bool {
    let Ok(parsed) = ast::parse::Parser::new().parse(pattern) else {
        return false;
    };
    InlineFlagAnalysis::visit(&parsed)
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
            Ast::Flags(ref x) => Self::flags(&x.flags),
            Ast::Repetition(ref x) => Self::visit(&x.ast),
            Ast::Group(ref x) => x.flags().is_some_and(Self::flags) || Self::visit(&x.ast),
            Ast::Alternation(ref x) => x.asts.iter().any(Self::visit),
            Ast::Concat(ref x) => x.asts.iter().any(Self::visit),
        }
    }

    fn flags(flags: &ast::Flags) -> bool {
        matches!(flags.flag_state(ast::Flag::CaseInsensitive), Some(true))
    }
}

/// Literal-character facts of a pattern AST, as `grep-regex` counts them.
#[derive(Default)]
struct CaseAnalysis {
    any_uppercase: bool,
    any_literal: bool,
}

impl CaseAnalysis {
    const fn done(&self) -> bool {
        self.any_uppercase && self.any_literal
    }

    fn visit(&mut self, node: &Ast) {
        if self.done() {
            return;
        }
        match *node {
            Ast::Empty(_)
            | Ast::Flags(_)
            | Ast::Dot(_)
            | Ast::Assertion(_)
            | Ast::ClassUnicode(_)
            | Ast::ClassPerl(_) => {},
            Ast::Literal(ref x) => self.literal(x),
            Ast::ClassBracketed(ref x) => self.class_set(&x.kind),
            Ast::Repetition(ref x) => self.visit(&x.ast),
            Ast::Group(ref x) => self.visit(&x.ast),
            Ast::Alternation(ref x) => {
                for sub in &x.asts {
                    self.visit(sub);
                }
            },
            Ast::Concat(ref x) => {
                for sub in &x.asts {
                    self.visit(sub);
                }
            },
        }
    }

    fn class_set(&mut self, set: &ClassSet) {
        if self.done() {
            return;
        }
        match *set {
            ClassSet::Item(ref item) => self.class_item(item),
            ClassSet::BinaryOp(ref op) => {
                self.class_set(&op.lhs);
                self.class_set(&op.rhs);
            },
        }
    }

    fn class_item(&mut self, item: &ClassSetItem) {
        if self.done() {
            return;
        }
        match *item {
            ClassSetItem::Empty(_)
            | ClassSetItem::Ascii(_)
            | ClassSetItem::Unicode(_)
            | ClassSetItem::Perl(_) => {},
            ClassSetItem::Literal(ref x) => self.literal(x),
            ClassSetItem::Range(ref x) => {
                self.literal(&x.start);
                self.literal(&x.end);
            },
            ClassSetItem::Bracketed(ref x) => self.class_set(&x.kind),
            ClassSetItem::Union(ref union) => {
                for sub in &union.items {
                    self.class_item(sub);
                }
            },
        }
    }

    const fn literal(&mut self, lit: &ast::Literal) {
        self.any_literal = true;
        self.any_uppercase = self.any_uppercase || lit.c.is_uppercase();
    }
}

#[cfg(test)]
mod tests {
    use super::{has_inline_case_insensitive, smart_case_insensitive};

    #[test]
    fn lowercase_literals_are_insensitive() {
        assert!(smart_case_insensitive("foo"));
        assert!(smart_case_insensitive("foo.*bar"));
    }

    #[test]
    fn uppercase_literals_are_sensitive() {
        assert!(!smart_case_insensitive("Foo"));
        assert!(!smart_case_insensitive("foo[A-Z]"));
        assert!(!smart_case_insensitive(r"foo\x41"));
    }

    #[test]
    fn uppercase_in_escapes_does_not_count() {
        assert!(smart_case_insensitive(r"foo\Wbar"));
        assert!(smart_case_insensitive(r"foo\Sbar"));
        assert!(smart_case_insensitive(r"\Afoo"));
        assert!(smart_case_insensitive(r"foo\pL"));
        assert!(smart_case_insensitive(r"(?P<Name>foo)"));
        assert!(smart_case_insensitive(r"foo\x6a"));
    }

    #[test]
    fn literal_free_patterns_stay_sensitive() {
        assert!(!smart_case_insensitive(r"\d{4}"));
        assert!(!smart_case_insensitive(r"\W+"));
    }

    #[test]
    fn unparseable_patterns_stay_sensitive() {
        assert!(!smart_case_insensitive("foo("));
    }

    #[test]
    fn detects_inline_case_insensitive_flags() {
        assert!(has_inline_case_insensitive("(?i)foo"));
        assert!(has_inline_case_insensitive("foo(?i:bar)baz"));
        assert!(has_inline_case_insensitive("(?:foo|(?i:bar))"));
        assert!(!has_inline_case_insensitive("foo"));
        assert!(!has_inline_case_insensitive("(?-i:foo)"));
        assert!(!has_inline_case_insensitive("foo("));
    }
}
