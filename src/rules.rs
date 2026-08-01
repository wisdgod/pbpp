//! Selector rules DSL.
//!
//! Line-oriented, gitignore-flavored:
//!
//! ```text
//! # keep the whole package, minus one message
//! + acme.api.v1.**
//! - acme.api.v1.LegacyThing
//!
//! # cascade: delete every field whose type is Any, reserving its number
//! -! google.protobuf.Any
//!
//! # scope block: prefix applies to nested rules
//! acme.api.v1 {
//!   + Search.Lookup @method
//!   - User.email @field
//! }
//! ```
//!
//! Semantics (fixed by the design):
//! - ordered rules, later rules override earlier ones (gitignore precedence);
//! - `+` keep / `-` exclude / `-!` exclude with cascade;
//! - patterns are dotted segments: literal, `*` (exactly one segment), or
//!   `**` (any number of segments); a rule also applies to everything nested
//!   under a matched node;
//! - optional `@kind` restricts the rule to one node kind;
//! - a rule that matches nothing is an error, an empty rule set is an error.

use crate::error::Error;
use crate::sema::SymKind;
use crate::span::Span;

/// What a matching rule does to a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Polarity {
    /// `+`: keep the node (and its subtree).
    Keep,
    /// `-`: exclude the node; an exclusion still reachable from the kept
    /// set is an error.
    Drop,
    /// `-!`: exclude and cascade — referencing fields/methods are deleted
    /// and their numbers reserved.
    DropCascade,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PatSeg {
    Lit(String),
    Star,
    DoubleStar,
}

/// One selector rule: polarity + path pattern + optional kind qualifier.
#[derive(Debug)]
pub struct Rule {
    /// Keep, drop, or drop-with-cascade.
    pub polarity: Polarity,
    pub(crate) pattern: Vec<PatSeg>,
    /// The `@kind` qualifier, if the rule has one.
    pub kind: Option<SymKind>,
    /// The rule as written (or as reconstructed for programmatic rules),
    /// for reports and error messages.
    pub raw: String,
    /// Span of the rule line in the rules source (`RuleSet::src`); `None`
    /// for programmatically built rules.
    pub(crate) span: Option<Span>,
}

impl Rule {
    /// True if the pattern matches the full segment path.
    #[must_use]
    pub fn matches_path(&self, segs: &[&str]) -> bool {
        glob(&self.pattern, segs)
    }
}

/// gitignore conventions: a trailing `**` matches one or more segments
/// (`a.**` matches `a`'s contents, not `a` itself); a `**` in the middle
/// matches zero or more (`a.**.z` matches `a.z`).
fn glob(pat: &[PatSeg], segs: &[&str]) -> bool {
    match pat.first() {
        None => segs.is_empty(),
        Some(PatSeg::DoubleStar) if pat.len() == 1 => !segs.is_empty(),
        Some(PatSeg::DoubleStar) => (0..=segs.len()).any(|k| glob(&pat[1..], &segs[k..])),
        Some(PatSeg::Star) => !segs.is_empty() && glob(&pat[1..], &segs[1..]),
        Some(PatSeg::Lit(l)) => segs.first() == Some(&l.as_str()) && glob(&pat[1..], &segs[1..]),
    }
}

/// An ordered rule list; later rules override earlier ones.
#[derive(Debug, Default)]
pub struct RuleSet {
    /// The rules, in declaration order.
    pub rules: Vec<Rule>,
    /// The rules source text, kept so selection errors about a rule can
    /// point at its line; `None` for programmatically built sets.
    pub(crate) src: Option<Box<str>>,
}

impl RuleSet {
    /// An empty rule set, for building rules programmatically. Selection
    /// treats an empty rule set as an error, same as an empty rules file.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rules: Vec::new(),
            src: None,
        }
    }

    /// Appends a rule. Order matters: later rules override earlier ones.
    ///
    /// # Errors
    ///
    /// Invalid pattern segments (each must be an identifier, `*`, or `**`).
    pub fn push(
        &mut self,
        polarity: Polarity,
        pattern: &str,
        kind: Option<SymKind>,
    ) -> Result<&mut Self, Error> {
        let segs = parse_pattern(pattern)
            .map_err(|m| Error::new(format!("invalid pattern `{pattern}`: {m}")))?;
        let marker = match polarity {
            Polarity::Keep => "+",
            Polarity::Drop => "-",
            Polarity::DropCascade => "-!",
        };
        let raw = kind.map_or_else(
            || format!("{marker} {pattern}"),
            |k| format!("{marker} {pattern} @{}", kind_name(k)),
        );
        self.rules.push(Rule {
            polarity,
            pattern: segs,
            kind,
            raw,
            span: None,
        });
        Ok(self)
    }

    /// Convenience for `push(Polarity::Keep, ..)`.
    ///
    /// # Errors
    ///
    /// See [`RuleSet::push`].
    pub fn keep(&mut self, pattern: &str) -> Result<&mut Self, Error> {
        self.push(Polarity::Keep, pattern, None)
    }

    /// Convenience for `push(Polarity::Drop, ..)`.
    ///
    /// # Errors
    ///
    /// See [`RuleSet::push`].
    pub fn drop(&mut self, pattern: &str) -> Result<&mut Self, Error> {
        self.push(Polarity::Drop, pattern, None)
    }

    /// Convenience for `push(Polarity::DropCascade, ..)` — the `-!` rule:
    /// referencing fields are deleted and their numbers reserved.
    ///
    /// # Errors
    ///
    /// See [`RuleSet::push`].
    pub fn drop_cascade(&mut self, pattern: &str) -> Result<&mut Self, Error> {
        self.push(Polarity::DropCascade, pattern, None)
    }
}

/// The `@kind` qualifier's vocabulary; inverse of [`kind_name`].
fn parse_kind(text: &str) -> Option<SymKind> {
    Some(match text {
        "message" => SymKind::Message,
        "enum" => SymKind::Enum,
        "service" => SymKind::Service,
        "field" => SymKind::Field,
        "method" => SymKind::Method,
        "value" => SymKind::EnumValue,
        _ => return None,
    })
}

const fn kind_name(k: SymKind) -> &'static str {
    match k {
        SymKind::Message => "message",
        SymKind::Enum => "enum",
        SymKind::Service => "service",
        SymKind::Field => "field",
        SymKind::Method => "method",
        SymKind::EnumValue => "value",
    }
}

/// Parses a selector rules file.
///
/// # Errors
///
/// Malformed lines (bad polarity marker, invalid pattern segment, unknown
/// `@kind`), unbalanced scope blocks, and an empty rule set are errors with
/// the offending line's span.
pub fn parse_rules(src: &str) -> Result<RuleSet, Error> {
    let mut rules = Vec::new();
    // Stack of scope prefixes; each entry also keeps the span of its opening
    // line for the unclosed-scope diagnostic.
    let mut scopes: Vec<(Vec<PatSeg>, Span)> = Vec::new();

    let mut offset = 0usize;
    for raw_line in src.split_inclusive('\n') {
        let line_start = offset;
        offset += raw_line.len();

        // Strip comment and whitespace, tracking the span of the content.
        let without_nl = raw_line.trim_end_matches(['\n', '\r']);
        let content = without_nl
            .find('#')
            .map_or(without_nl, |i| &without_nl[..i]);
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        let col = content.len() - content.trim_start().len();
        let span = Span::new(line_start + col, line_start + col + trimmed.len());

        if trimmed == "}" {
            if scopes.pop().is_none() {
                return Err(Error::at("unmatched `}` (no open scope)", span, src));
            }
            continue;
        }

        if let Some(prefix_text) = trimmed.strip_suffix('{') {
            let prefix_text = prefix_text.trim();
            if prefix_text.is_empty() {
                return Err(Error::at(
                    "scope block needs a path prefix before `{`",
                    span,
                    src,
                ));
            }
            let prefix = parse_pattern(prefix_text).map_err(|m| Error::at(m, span, src))?;
            // Entries on the stack already carry their full accumulated
            // prefix, so only the innermost one is extended.
            let mut full: Vec<PatSeg> = scopes.last().map(|(p, _)| p.clone()).unwrap_or_default();
            full.extend(prefix);
            scopes.push((full, span));
            continue;
        }

        // A rule line.
        let (polarity, rest) = if let Some(r) = trimmed.strip_prefix("-!") {
            (Polarity::DropCascade, r)
        } else if let Some(r) = trimmed.strip_prefix('-') {
            (Polarity::Drop, r)
        } else if trimmed.starts_with("+!") {
            return Err(Error::at(
                "the cascade marker `!` only applies to `-` (exclusion) rules",
                span,
                src,
            ));
        } else if let Some(r) = trimmed.strip_prefix('+') {
            (Polarity::Keep, r)
        } else {
            return Err(Error::at(
                format!(
                    "expected a rule (`+`, `-`, `-!`), a scope block, or `}}`, found `{trimmed}`"
                ),
                span,
                src,
            ));
        };

        let rest = rest.trim();
        let (pattern_text, kind) = match rest.split_once('@') {
            Some((p, k)) => {
                let kind = parse_kind(k.trim()).ok_or_else(|| {
                    Error::at(
                        format!(
                            "unknown kind `@{}` (expected one of `message`, `enum`, `service`, `field`, `method`, `value`)",
                            k.trim()
                        ),
                        span,
                        src,
                    )
                })?;
                (p.trim(), Some(kind))
            }
            None => (rest, None),
        };
        if pattern_text.is_empty() {
            return Err(Error::at("rule is missing a path pattern", span, src));
        }

        let mut pattern: Vec<PatSeg> = scopes.last().map(|(p, _)| p.clone()).unwrap_or_default();
        pattern.extend(parse_pattern(pattern_text).map_err(|m| Error::at(m, span, src))?);

        rules.push(Rule {
            polarity,
            pattern,
            kind,
            raw: trimmed.to_string(),
            span: Some(span),
        });
    }

    if let Some((_, span)) = scopes.last() {
        return Err(Error::at("unclosed scope block (missing `}`)", *span, src));
    }
    if rules.is_empty() {
        return Err(Error::new("selector configuration contains no rules"));
    }
    Ok(RuleSet {
        rules,
        src: Some(src.into()),
    })
}

/// Parses a dotted pattern into segments; errors are plain messages so
/// both the DSL parser and the programmatic API can attach their own
/// context.
fn parse_pattern(text: &str) -> Result<Vec<PatSeg>, String> {
    let mut out = Vec::new();
    for seg in text.split('.') {
        let seg = seg.trim();
        if seg == "*" {
            out.push(PatSeg::Star);
        } else if seg == "**" {
            out.push(PatSeg::DoubleStar);
        } else if !seg.is_empty()
            && seg
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            out.push(PatSeg::Lit(seg.to_string()));
        } else {
            return Err(format!(
                "invalid pattern segment `{seg}` (expected an identifier, `*`, or `**`)"
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(s: &str) -> PatSeg {
        PatSeg::Lit(s.to_string())
    }

    #[test]
    fn parses_rules_and_scopes() {
        let src = "\
# comment
+ a.b.**
-! google.protobuf.Any

a.b {
  + User @message
  c {
    - D.e @field
  }
}
";
        let rs = parse_rules(src).unwrap();
        assert_eq!(rs.rules.len(), 4);
        assert_eq!(rs.rules[0].polarity, Polarity::Keep);
        assert_eq!(
            rs.rules[0].pattern,
            vec![seg("a"), seg("b"), PatSeg::DoubleStar]
        );
        assert_eq!(rs.rules[1].polarity, Polarity::DropCascade);
        assert_eq!(rs.rules[2].kind, Some(crate::sema::SymKind::Message));
        assert_eq!(rs.rules[2].pattern, vec![seg("a"), seg("b"), seg("User")]);
        assert_eq!(
            rs.rules[3].pattern,
            vec![seg("a"), seg("b"), seg("c"), seg("D"), seg("e")]
        );
        assert_eq!(rs.rules[3].polarity, Polarity::Drop);
    }

    #[test]
    fn glob_semantics() {
        let r = |p: &str| parse_rules(&format!("+ {p}\n")).unwrap().rules.remove(0);
        assert!(r("a.b.C").matches_path(&["a", "b", "C"]));
        assert!(!r("a.b.C").matches_path(&["a", "b", "C", "d"]));
        assert!(r("a.*.C").matches_path(&["a", "b", "C"]));
        assert!(!r("a.*.C").matches_path(&["a", "b", "b2", "C"]));
        assert!(r("a.**").matches_path(&["a", "b", "C"]));
        // Trailing `**` matches contents, not the node itself (gitignore).
        assert!(!r("a.**").matches_path(&["a"]));
        assert!(r("**.C").matches_path(&["a", "b", "C"]));
        // A `**` in the middle matches zero or more segments.
        assert!(r("a.**.z").matches_path(&["a", "z"]));
    }

    #[test]
    fn errors() {
        assert!(parse_rules("").is_err());
        assert!(parse_rules("# only comments\n").is_err());
        assert!(parse_rules("+ a @widget\n").is_err());
        assert!(parse_rules("a.b {\n+ c\n").is_err());
        assert!(parse_rules("}\n").is_err());
        assert!(parse_rules("+! a\n").is_err());
        assert!(parse_rules("+ a..b\n").is_err());
        assert!(parse_rules("keep a.b\n").is_err());
    }
}
