//! Planner options mirroring the verifier's matcher configuration.
//!
//! The gram plan is only sound when it is built from the same match
//! semantics the verifying engine uses. [`PlanOptions`] carries the
//! semantics-bearing knobs; anything affecting only output, traversal, or
//! engine resource limits stays out on purpose. Word/line anchoring is also
//! absent: it only restricts the language, so a plan built without it is
//! already a sound superset (and a boundary the byte context refutes still
//! proves the pattern empty). Anchor flavors that change WHICH byte counts
//! as a line terminator do matter — that is the `crlf` field.

use regex_syntax::ast::{self, Ast, ClassSet, ClassSetItem};

/// How the verifying engine interprets the pattern text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PlanSyntax {
    /// Rust `regex` crate syntax, the engine default.
    #[default]
    Regex,
    /// Patterns are literal strings (`grep -F`); metacharacters are escaped
    /// before parsing, exactly as `grep-regex` does.
    FixedStrings,
}

/// The verifying engine's case mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PlanCase {
    /// Match case sensitively.
    #[default]
    Sensitive,
    /// Match case insensitively.
    Insensitive,
    /// Insensitive when no pattern contains an uppercase literal
    /// (`ripgrep -S`); resolved here with the same rule `grep-regex` applies,
    /// so the plan and the verifier can never disagree.
    Smart,
}

/// Options a caller passes alongside its patterns; every field must mirror
/// the engine that verifies candidates, or the plan may drop real matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool mirrors an independent matcher flag; a state machine would misrepresent them"
)]
pub struct PlanOptions {
    /// Pattern text interpretation.
    pub syntax: PlanSyntax,
    /// Case mode, including engine-rule smart case.
    pub case: PlanCase,
    /// Unicode mode (`--no-unicode` disables).
    pub unicode: bool,
    /// Whether `.` matches line terminators (`-U --multiline-dotall`).
    /// Currently plan-inert — `.` over-approximates to any character either
    /// way — but kept so the planner's HIR always equals the verifier's.
    pub dotall: bool,
    /// CRLF-aware anchors (`--crlf`).
    pub crlf: bool,
    /// Match-sense inversion (`-v`). The only sound prefilter for "lines
    /// that do NOT match" is every document, so this forces
    /// [`super::QueryPlan::All`].
    pub invert: bool,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            syntax: PlanSyntax::default(),
            case: PlanCase::default(),
            unicode: true,
            dotall: false,
            crlf: false,
            invert: false,
        }
    }
}

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
    use super::smart_case_insensitive;

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
}
