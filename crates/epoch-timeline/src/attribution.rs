//! [`Attribution`] — the schema'd JSON payload a checkpoint event carries in
//! [`TimelineEvent::meta`](crate::TimelineEvent) to record *which mount*
//! produced it.
//!
//! The timeline core treats `meta` as a free-form `Option<String>`; this
//! module is the first (and so far only) schema committed to that field. It
//! exists for the audit surface: `epoch audit` derives a per-lane write log
//! from consecutive checkpoint events, and per-lane attribution is the point —
//! the parent maps `mount` → lane at spawn time.
//!
//! ## Schema v1
//!
//! ```json
//! {"v":1,"mount":"m-9f2c…","head":"main","user":"chris","host":"mac","lower":"/abs/lower"}
//! ```
//!
//! - `v` — schema version (required; anything else ⇒ not an attribution meta).
//! - `mount` — the mount's identity, generated once per mount (required).
//! - `head` — the timeline head the mount advances (required).
//! - `user`, `host` — best-effort environment identity (optional).
//! - `lower` — absolute path of the overlaid lower directory, when the mount
//!   has one (optional; store-side tools use it to default `--lower`).
//!
//! Old events carry no meta at all (`None`) and old metas carry free text —
//! [`Attribution::parse`] answers `None` for anything that is not a schema-v1
//! attribution, so mixed history decodes fine.
//!
//! The JSON is hand-rolled on purpose: the workspace has no serde dependency
//! and the schema is a flat string map, so a strict ~100-line codec with
//! round-trip tests beats a dependency for a contract this small.

use std::fmt::Write as _;

/// The attribution schema version stamped as `"v"` in every meta payload.
pub const ATTRIBUTION_SCHEMA_VERSION: u32 = 1;

/// The identity of the mount that produced a checkpoint event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    /// Mount identity, generated once at mount time (e.g. `m-<16 hex>`).
    pub mount: String,
    /// The timeline head the mount advances.
    pub head: String,
    /// Best-effort user name (`$USER`), when known.
    pub user: Option<String>,
    /// Best-effort host name, when known.
    pub host: Option<String>,
    /// Absolute path of the overlaid lower directory, when the mount has one.
    pub lower: Option<String>,
}

impl Attribution {
    /// Encode as the schema'd JSON string stored in `TimelineEvent::meta`.
    pub fn to_meta_json(&self) -> String {
        let mut out = String::new();
        out.push('{');
        let _ = write!(out, "\"v\":{ATTRIBUTION_SCHEMA_VERSION}");
        write_field(&mut out, "mount", &self.mount);
        write_field(&mut out, "head", &self.head);
        if let Some(user) = &self.user {
            write_field(&mut out, "user", user);
        }
        if let Some(host) = &self.host {
            write_field(&mut out, "host", host);
        }
        if let Some(lower) = &self.lower {
            write_field(&mut out, "lower", lower);
        }
        out.push('}');
        out
    }

    /// Parse a `meta` payload as a schema-v1 attribution. Answers `None` for
    /// absent metas, free-text metas, other schema versions, and malformed
    /// JSON — callers must treat attribution as best-effort.
    pub fn parse(meta: &str) -> Option<Attribution> {
        let fields = parse_flat_json_object(meta)?;
        if fields.iter().find(|(k, _)| k == "v")?.1 != "1" {
            return None;
        }
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        Some(Attribution {
            mount: get("mount")?,
            head: get("head")?,
            user: get("user"),
            host: get("host"),
            lower: get("lower"),
        })
    }
}

fn write_field(out: &mut String, key: &str, value: &str) {
    out.push(',');
    escape_json_string_into(key, out);
    out.push(':');
    escape_json_string_into(value, out);
}

/// Append `s` as a quoted, escaped JSON string literal.
pub fn escape_json_string_into(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse a flat JSON object of string/number values into key→value pairs
/// (numbers are captured as their literal text). Strict: anything nested,
/// malformed, or trailing rejects the whole input.
fn parse_flat_json_object(input: &str) -> Option<Vec<(String, String)>> {
    let mut p = Parser {
        chars: input.chars().peekable(),
    };
    p.ws();
    p.expect('{')?;
    let mut fields = Vec::new();
    p.ws();
    if p.peek() == Some('}') {
        p.expect('}');
        p.ws();
        return p.at_end().then_some(fields);
    }
    loop {
        p.ws();
        let key = p.string()?;
        p.ws();
        p.expect(':')?;
        p.ws();
        let value = match p.peek()? {
            '"' => p.string()?,
            c if c.is_ascii_digit() => p.number()?,
            _ => return None,
        };
        fields.push((key, value));
        p.ws();
        match p.peek()? {
            ',' => {
                p.expect(',');
            }
            '}' => {
                p.expect('}');
                break;
            }
            _ => return None,
        }
    }
    p.ws();
    p.at_end().then_some(fields)
}

struct Parser<I: Iterator<Item = char>> {
    chars: std::iter::Peekable<I>,
}

impl<I: Iterator<Item = char>> Parser<I> {
    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    fn at_end(&mut self) -> bool {
        self.chars.peek().is_none()
    }

    fn ws(&mut self) {
        while matches!(self.chars.peek(), Some(c) if c.is_whitespace()) {
            self.chars.next();
        }
    }

    fn expect(&mut self, want: char) -> Option<()> {
        (self.chars.next()? == want).then_some(())
    }

    fn number(&mut self) -> Option<String> {
        let mut out = String::new();
        while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
            out.push(self.chars.next()?);
        }
        (!out.is_empty()).then_some(out)
    }

    fn string(&mut self) -> Option<String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.chars.next()? {
                '"' => return Some(out),
                '\\' => match self.chars.next()? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let hi = self.hex4()?;
                        let ch = if (0xd800..0xdc00).contains(&hi) {
                            // Surrogate pair: combine with the low surrogate.
                            self.expect('\\')?;
                            self.expect('u')?;
                            let lo = self.hex4()?;
                            if !(0xdc00..0xe000).contains(&lo) {
                                return None;
                            }
                            let cp = 0x1_0000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
                            char::from_u32(cp)?
                        } else {
                            char::from_u32(hi)?
                        };
                        out.push(ch);
                    }
                    _ => return None,
                },
                c if (c as u32) < 0x20 => return None, // raw control chars are invalid JSON
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            v = v * 16 + self.chars.next()?.to_digit(16)?;
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Attribution {
        Attribution {
            mount: "m-0123456789abcdef".into(),
            head: "main".into(),
            user: Some("chris".into()),
            host: Some("mac".into()),
            lower: Some("/Users/chris/project".into()),
        }
    }

    #[test]
    fn roundtrips_full_and_sparse() {
        let full = sample();
        assert_eq!(
            Attribution::parse(&full.to_meta_json()),
            Some(full.clone())
        );

        let sparse = Attribution {
            user: None,
            host: None,
            lower: None,
            ..full
        };
        let json = sparse.to_meta_json();
        assert_eq!(json, "{\"v\":1,\"mount\":\"m-0123456789abcdef\",\"head\":\"main\"}");
        assert_eq!(Attribution::parse(&json), Some(sparse));
    }

    #[test]
    fn roundtrips_escapes_and_unicode() {
        let tricky = Attribution {
            mount: "m-\"quoted\"\\back\tslash\nnewline".into(),
            head: "héad→ snowman ☃".into(),
            user: None,
            host: Some("𝕳𝖔𝖘𝖙".into()), // astral plane: surrogate pair in JSON
            lower: None,
        };
        assert_eq!(Attribution::parse(&tricky.to_meta_json()), Some(tricky));
    }

    #[test]
    fn parse_rejects_non_attribution_metas() {
        assert_eq!(Attribution::parse(""), None);
        assert_eq!(Attribution::parse("free text"), None);
        assert_eq!(Attribution::parse("{}"), None, "no version");
        assert_eq!(
            Attribution::parse("{\"v\":2,\"mount\":\"m\",\"head\":\"h\"}"),
            None,
            "other schema version"
        );
        assert_eq!(
            Attribution::parse("{\"v\":1,\"head\":\"h\"}"),
            None,
            "missing required mount"
        );
        assert_eq!(
            Attribution::parse("{\"v\":1,\"mount\":\"m\",\"head\":\"h\"} trailing"),
            None,
            "trailing garbage"
        );
        assert_eq!(
            Attribution::parse("{\"v\":1,\"mount\":\"m\",\"head\":{\"x\":1}}"),
            None,
            "nested values are not flat"
        );
    }

    #[test]
    fn parse_tolerates_unknown_fields_and_ws() {
        let json = "{\n  \"v\": 1, \"mount\":\"m\", \"future\": [\"ignored-ish\"], \"head\":\"h\"\n}";
        // Unknown *nested* fields still reject (flat schema); flat strings pass.
        assert_eq!(Attribution::parse(json), None);
        let flat = "{ \"v\":1, \"future\":\"ok\", \"mount\":\"m\", \"head\":\"h\" }";
        let parsed = Attribution::parse(flat).unwrap();
        assert_eq!(parsed.mount, "m");
        assert_eq!(parsed.head, "h");
        assert_eq!(parsed.lower, None);
    }
}
