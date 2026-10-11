use crate::{
    directive::{Directive, validate_name},
    discovery::DependencyFeatures,
};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};

pub fn generate(
    directives: &[Directive],
    dependencies: &[DependencyFeatures],
    manual_features: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut claimed = BTreeMap::<String, &Directive>::new();
    let mut generated = Vec::with_capacity(directives.len());

    for (directive, dependency) in directives.iter().zip(dependencies) {
        let mut features = Vec::new();
        for dependency_feature in &dependency.names {
            // `default` is deliberately excluded: forwarding it would force a behavior
            // change in the dependency, and Cargo already enables defaults by default.
            if dependency_feature == "default" || directive.exclude.contains(dependency_feature) {
                continue;
            }
            let name = format!("{}{}", directive.prefix, dependency_feature);
            validate_name(&name, "generated feature name").with_context(|| {
                format!(
                    "line {}: prefix `{}` and dependency feature `{dependency_feature}` do not form a valid Cargo feature name",
                    directive.line, directive.prefix
                )
            })?;
            if manual_features.contains(&name) {
                bail!(
                    "line {}: generated feature `{name}` for dependency `{}` collides with a manually defined feature",
                    directive.line,
                    directive.dependency
                );
            }
            if let Some(previous) = claimed.insert(name.clone(), directive) {
                bail!(
                    "line {}: generated feature `{name}` for dependency `{}` collides with forwarding block on line {} for dependency `{}`",
                    directive.line,
                    directive.dependency,
                    previous.line,
                    previous.dependency
                );
            }
            let dependency_reference = if dependency.optional {
                format!("{}?/{}", directive.dependency, dependency_feature)
            } else {
                format!("{}/{}", directive.dependency, dependency_feature)
            };
            features.push((name, dependency_reference));
        }
        features.sort_by(|left, right| left.0.cmp(&right.0));
        generated.push(
            features
                .into_iter()
                .map(|(name, reference)| format!("{name} = [\"{reference}\"]"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    Ok(generated)
}
