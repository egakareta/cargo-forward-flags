use anyhow::{Result, bail};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    pub dependency: String,
    pub prefix: String,
    pub exclude: BTreeSet<String>,
    pub line: usize,
    pub source: String,
}

pub fn parse_directive(text: &str, line: usize) -> Result<Directive> {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("<forward") else {
        bail!("expected a `<forward ...>` directive");
    };
    if !rest.chars().next().is_some_and(char::is_whitespace) && !rest.starts_with('>') {
        bail!("malformed forwarding directive; expected `<forward DEPENDENCY ...>`");
    }
    let Some(rest) = rest.strip_suffix('>') else {
        bail!("malformed forwarding directive; expected a closing `>`");
    };
    if rest.contains('>') || rest.contains('<') {
        bail!("malformed forwarding directive; unexpected angle bracket");
    }

    let mut cursor = 0;
    skip_space(rest, &mut cursor);
    let dependency = take_word(rest, &mut cursor);
    if dependency.is_empty() {
        bail!("forwarding directive is missing a dependency key");
    }
    validate_name(dependency, "dependency key")?;

    let mut prefix = None;
    let mut exclude = None;
    loop {
        skip_space(rest, &mut cursor);
        if cursor == rest.len() {
            break;
        }

        let key_start = cursor;
        while cursor < rest.len()
            && !rest.as_bytes()[cursor].is_ascii_whitespace()
            && rest.as_bytes()[cursor] != b'='
        {
            cursor += 1;
        }
        let key = &rest[key_start..cursor];
        if key.is_empty() {
            bail!("malformed attribute in forwarding directive");
        }
        skip_space(rest, &mut cursor);
        if rest.as_bytes().get(cursor) != Some(&b'=') {
            bail!("attribute `{key}` must have a quoted value (for example `{key}=\"value\"`)");
        }
        cursor += 1;
        skip_space(rest, &mut cursor);
        let value = take_quoted(rest, &mut cursor)?;

        match key {
            "prefix" => {
                if prefix.replace(value).is_some() {
                    bail!("duplicate `prefix` attribute");
                }
            }
            "exclude" => {
                if exclude.is_some() {
                    bail!("duplicate `exclude` attribute");
                }
                let mut excluded = BTreeSet::new();
                if !value.is_empty() {
                    for name in value.split(',') {
                        let name = name.trim();
                        if name.is_empty() {
                            bail!("`exclude` contains an empty feature name");
                        }
                        validate_name(name, "excluded feature")?;
                        excluded.insert(name.to_owned());
                    }
                }
                exclude = Some(excluded);
            }
            _ => bail!("unknown forwarding attribute `{key}`"),
        }
    }

    let prefix = prefix.unwrap_or_default();
    if !prefix.is_empty()
        && !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("`prefix` must contain only ASCII letters, digits, `_`, and `-`");
    }

    Ok(Directive {
        dependency: dependency.to_owned(),
        prefix,
        exclude: exclude.unwrap_or_default(),
        line,
        source: text.to_owned(),
    })
}

pub fn validate_name(name: &str, description: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("{description} `{name}` is not a valid Cargo feature/dependency name");
    }
    Ok(())
}

fn skip_space(text: &str, cursor: &mut usize) {
    while *cursor < text.len() && text.as_bytes()[*cursor].is_ascii_whitespace() {
        *cursor += 1;
    }
}

fn take_word<'a>(text: &'a str, cursor: &mut usize) -> &'a str {
    let start = *cursor;
    while *cursor < text.len() && !text.as_bytes()[*cursor].is_ascii_whitespace() {
        *cursor += 1;
    }
    &text[start..*cursor]
}

fn take_quoted(text: &str, cursor: &mut usize) -> Result<String> {
    if text.as_bytes().get(*cursor) != Some(&b'"') {
        bail!("attribute values must be double-quoted");
    }
    *cursor += 1;
    let mut value = String::new();
    while *cursor < text.len() {
        let byte = text.as_bytes()[*cursor];
        *cursor += 1;
        match byte {
            b'"' => return Ok(value),
            b'\\' => {
                let Some(escaped) = text.as_bytes().get(*cursor).copied() else {
                    bail!("unterminated escape in attribute value");
                };
                *cursor += 1;
                match escaped {
                    b'\\' => value.push('\\'),
                    b'"' => value.push('"'),
                    _ => bail!("unsupported escape in attribute value; use `\\` or `\"`"),
                }
            }
            byte if byte.is_ascii() => value.push(byte as char),
            _ => {
                let start = *cursor - 1;
                let character = text[start..].chars().next().expect("valid UTF-8");
                value.push(character);
                *cursor = start + character.len_utf8();
            }
        }
    }
    bail!("unterminated quoted attribute value")
}

#[cfg(test)]
mod tests {
    use super::parse_directive;

    #[test]
    fn parses_attributes_in_either_order() {
        let a = parse_directive("<forward serde exclude=\"rc,std\" prefix=\"serde-\">", 3).unwrap();
        let b = parse_directive("<forward serde prefix=\"serde-\" exclude=\"std,rc\">", 3).unwrap();
        assert_eq!(a.dependency, b.dependency);
        assert_eq!(a.prefix, b.prefix);
        assert_eq!(a.exclude, b.exclude);
        assert_eq!(a.dependency, "serde");
        assert!(a.exclude.contains("rc"));
    }

    #[test]
    fn rejects_duplicate_unknown_and_malformed_attributes() {
        for text in [
            "<forward serde prefix=\"a\" prefix=\"b\">",
            "<forward serde unknown=\"x\">",
            "<forward serde prefix=x>",
            "<forward>",
            "<forward serde",
        ] {
            assert!(parse_directive(text, 1).is_err(), "accepted {text}");
        }
    }
}
