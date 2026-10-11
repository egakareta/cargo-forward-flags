use crate::directive::{Directive, parse_directive};
use anyhow::{Context, Result, bail};
use std::{collections::BTreeSet, fs, io::Write, path::Path};

#[derive(Debug, Clone)]
pub struct ForwardBlock {
    pub directive: Directive,
    pub content_start: usize,
    pub content_end: usize,
}

pub struct ParsedManifest {
    pub blocks: Vec<ForwardBlock>,
    pub manual_features: BTreeSet<String>,
    pub newline: &'static str,
}

pub fn parse_manifest(text: &str) -> Result<ParsedManifest> {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut blocks = Vec::new();
    let mut current_section = String::new();
    let mut open: Option<(Directive, usize)> = None;
    let mut offset = 0;

    for (index, part) in text.split_inclusive('\n').enumerate() {
        let line_number = index + 1;
        let line = part
            .strip_suffix('\n')
            .unwrap_or(part)
            .strip_suffix('\r')
            .unwrap_or_else(|| part.strip_suffix('\n').unwrap_or(part));
        let trimmed = line.trim();

        if let Some(comment) = trimmed.strip_prefix('#') {
            let marker = comment.trim_start();
            if marker.starts_with("<forward") {
                if let Some((directive, _)) = &open {
                    bail!(
                        "line {line_number}: nested forwarding block inside directive `{}` from line {}",
                        directive.source,
                        directive.line
                    );
                }
                if current_section != "features" {
                    bail!(
                        "line {line_number}: forwarding blocks must be inside the top-level `[features]` table"
                    );
                }
                let directive = parse_directive(marker, line_number).with_context(|| {
                    format!("line {line_number}: invalid forwarding directive `{marker}`")
                })?;
                let content_start = offset + part.len();
                open = Some((directive, content_start));
            } else if marker.starts_with("</forward") {
                let Some((directive, content_start)) = open.take() else {
                    bail!("line {line_number}: unmatched forwarding closing marker");
                };
                if marker != "</forward>" {
                    bail!(
                        "line {line_number}: malformed closing marker for directive `{}`; expected `# </forward>`",
                        directive.source
                    );
                }
                blocks.push(ForwardBlock {
                    directive,
                    content_start,
                    content_end: offset,
                });
            }
        } else if trimmed.starts_with('[') {
            if let Some((directive, _)) = &open {
                bail!(
                    "line {line_number}: forwarding block `{}` from line {} crosses a TOML table boundary",
                    directive.source,
                    directive.line
                );
            }
            current_section = table_name(trimmed).unwrap_or_default();
        }
        offset += part.len();
    }

    if let Some((directive, _)) = open {
        bail!(
            "line {}: forwarding directive `{}` has no matching `# </forward>` marker",
            directive.line,
            directive.source
        );
    }

    let sanitized = blank_managed_regions(text, &blocks);
    let document: toml::Value = toml::from_str(&sanitized)
        .context("manifest is not valid TOML outside forwarding blocks")?;
    let features = document
        .get("features")
        .and_then(toml::Value::as_table)
        .context("forwarding blocks require a top-level `[features]` table")?;
    let manual_features = features.keys().cloned().collect();

    Ok(ParsedManifest {
        blocks,
        manual_features,
        newline,
    })
}

fn table_name(line: &str) -> Option<String> {
    if line.starts_with("[[") {
        return Some(String::new());
    }
    let end = line.find(']')?;
    let remainder = line[end + 1..].trim();
    if !remainder.is_empty() && !remainder.starts_with('#') {
        return Some(String::new());
    }
    Some(line[1..end].trim().to_owned())
}

fn blank_managed_regions(text: &str, blocks: &[ForwardBlock]) -> String {
    let mut bytes = text.as_bytes().to_vec();
    for block in blocks {
        for byte in &mut bytes[block.content_start..block.content_end] {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    // Replacing UTF-8 bytes with ASCII spaces preserves valid UTF-8 and all offsets.
    String::from_utf8(bytes).expect("spaces and preserved UTF-8 line endings remain valid")
}

pub fn rewrite(text: &str, parsed: &ParsedManifest, generated: &[String]) -> Result<String> {
    if parsed.blocks.len() != generated.len() {
        bail!("internal error: generated feature blocks do not match parsed directives");
    }
    let mut updated = text.to_owned();
    let mut replacements: Vec<_> = parsed
        .blocks
        .iter()
        .zip(generated)
        .map(|(block, contents)| {
            let replacement = if contents.is_empty() {
                String::new()
            } else {
                format!("{contents}{}", parsed.newline)
            };
            (block.content_start, block.content_end, replacement)
        })
        .collect();
    replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0));
    for (start, end, replacement) in replacements {
        updated.replace_range(start..end, &replacement);
    }

    let _: toml::Value = toml::from_str(&updated)
        .context("generated content did not produce a valid TOML manifest")?;
    Ok(updated)
}

pub fn write_atomically(path: &Path, expected: &str, updated: &str) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("could not inspect manifest `{}`", path.display()))?;
    let permissions = metadata.permissions();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .context("manifest path has no file name")?
        .to_string_lossy();

    let mut temporary = None;
    for attempt in 0..100 {
        let candidate = parent.join(format!(
            ".{name}.cargo-forward-flags-{}-{attempt}.tmp",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "could not create temporary file beside `{}`",
                        path.display()
                    )
                });
            }
        }
    }
    let (temporary_path, mut file) =
        temporary.context("could not allocate a temporary manifest file")?;
    let result = (|| -> Result<()> {
        file.write_all(updated.as_bytes())
            .context("could not write updated manifest")?;
        file.sync_all()
            .context("could not flush updated manifest")?;
        file.set_permissions(permissions.clone())
            .context("could not preserve manifest permissions")?;
        drop(file);

        let current = fs::read_to_string(path)
            .with_context(|| format!("could not re-read manifest `{}`", path.display()))?;
        if current != expected {
            bail!(
                "manifest `{}` changed while cargo-forward-flags was running; no changes were written",
                path.display()
            );
        }
        fs::rename(&temporary_path, path)
            .with_context(|| format!("could not atomically replace `{}`", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

pub fn diff(path: &Path, parsed: &ParsedManifest, generated: &[String], original: &str) -> String {
    let mut output = format!(
        "--- {}\n+++ {} (generated)\n",
        path.display(),
        path.display()
    );
    for (block, replacement) in parsed.blocks.iter().zip(generated) {
        let old = &original[block.content_start..block.content_end];
        let new = if replacement.is_empty() {
            String::new()
        } else {
            format!("{replacement}{}", parsed.newline)
        };
        if old == new {
            continue;
        }
        output.push_str(&format!(
            "@@ line {}: {} @@\n",
            block.directive.line, block.directive.source
        ));
        for line in old.lines() {
            output.push('-');
            output.push_str(line);
            output.push('\n');
        }
        for line in new.lines() {
            output.push('+');
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{parse_manifest, rewrite};
    use crate::discovery::DependencyFeatures;
    use crate::generate::generate;

    #[test]
    fn replaces_only_managed_text_and_is_idempotent() {
        let source = "[package]\nname = \"x\" # keep\nversion = \"0.1.0\"\n\n[features]\nmanual = []\n# <forward dep prefix=\"p-\">\nold = [\"dep/old\"]\n# </forward>  \n";
        let parsed = parse_manifest(source).unwrap();
        let generated = generate(
            &[parsed.blocks[0].directive.clone()],
            &[DependencyFeatures {
                names: ["b", "a", "default"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                optional: false,
            }],
            &parsed.manual_features,
        )
        .unwrap();
        let once = rewrite(source, &parsed, &generated).unwrap();
        let parsed_again = parse_manifest(&once).unwrap();
        let generated_again = generate(
            &[parsed_again.blocks[0].directive.clone()],
            &[DependencyFeatures {
                names: ["b", "a", "default"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                optional: false,
            }],
            &parsed_again.manual_features,
        )
        .unwrap();
        let twice = rewrite(&once, &parsed_again, &generated_again).unwrap();
        assert_eq!(once, twice);
        assert!(once.contains("p-a = [\"dep/a\"]\np-b = [\"dep/b\"]\n"));
        assert!(once.contains("name = \"x\" # keep"));
        assert!(once.contains("# </forward>  \n"));
        assert!(!once.contains("p-default"));
    }

    #[test]
    fn rejects_unmatched_nested_and_out_of_table_markers() {
        for text in [
            "# </forward>\n",
            "[features]\n# <forward dep>\n# <forward dep2>\n# </forward>\n# </forward>\n",
            "[package]\n# <forward dep>\n# </forward>\n",
            "[features]\n# <forward dep>\n",
        ] {
            assert!(parse_manifest(text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn detects_manual_feature_collisions() {
        let source = "[features]\na = []\n# <forward dep>\n# </forward>\n";
        let parsed = parse_manifest(source).unwrap();
        let result = generate(
            &[parsed.blocks[0].directive.clone()],
            &[DependencyFeatures {
                names: ["a"].into_iter().map(str::to_owned).collect(),
                optional: false,
            }],
            &parsed.manual_features,
        );
        assert!(result.unwrap_err().to_string().contains("collides"));
    }

    #[test]
    fn detects_collisions_between_forwarding_blocks() {
        let source =
            "[features]\n# <forward first>\n# </forward>\n# <forward second>\n# </forward>\n";
        let parsed = parse_manifest(source).unwrap();
        let features = DependencyFeatures {
            names: ["shared"].into_iter().map(str::to_owned).collect(),
            optional: false,
        };
        let result = generate(
            &parsed
                .blocks
                .iter()
                .map(|block| block.directive.clone())
                .collect::<Vec<_>>(),
            &[features.clone(), features],
            &parsed.manual_features,
        );
        assert!(result.unwrap_err().to_string().contains("forwarding block"));
    }
}
