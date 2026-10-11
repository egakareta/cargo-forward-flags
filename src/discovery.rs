use crate::directive::Directive;
use anyhow::{Context, Result, bail};
use cargo_metadata::{MetadataCommand, Package};
use std::{collections::BTreeSet, path::Path};

#[derive(Debug, Clone)]
pub struct DependencyFeatures {
    pub names: BTreeSet<String>,
    pub optional: bool,
}

pub fn discover(manifest_path: &Path, directives: &[Directive]) -> Result<Vec<DependencyFeatures>> {
    let mut command = MetadataCommand::new();
    command.manifest_path(manifest_path);
    // Activating all features makes Cargo include optional dependency packages in the
    // resolved graph. We only read package metadata; no build is performed.
    command.features(cargo_metadata::CargoOpt::AllFeatures);
    let metadata = command.exec().with_context(|| {
        format!(
            "could not resolve dependencies for manifest `{}` using `cargo metadata --all-features`",
            manifest_path.display()
        )
    })?;

    let manifest = manifest_path.canonicalize().with_context(|| {
        format!(
            "could not resolve manifest path `{}`",
            manifest_path.display()
        )
    })?;
    let root_package = metadata
        .packages
        .iter()
        .find(|package| package.manifest_path.as_std_path() == manifest)
        .context("Cargo metadata did not return the package described by this manifest")?;
    let resolve = metadata
        .resolve
        .as_ref()
        .context("Cargo metadata returned no dependency resolution graph")?;
    let root_node = resolve
        .nodes
        .iter()
        .find(|node| node.id == root_package.id)
        .context("Cargo metadata returned no resolved node for the current package")?;

    directives
        .iter()
        .map(|directive| discover_one(root_package, root_node, &metadata.packages, directive))
        .collect()
}

fn discover_one(
    root_package: &Package,
    root_node: &cargo_metadata::Node,
    packages: &[Package],
    directive: &Directive,
) -> Result<DependencyFeatures> {
    let matching: Vec<_> = root_package
        .dependencies
        .iter()
        .filter(|dependency| {
            dependency.rename.as_deref().unwrap_or(&dependency.name) == directive.dependency
        })
        .collect();

    if matching.is_empty() {
        bail!(
            "{}: forwarding directive `{}` names `{}`, but that dependency key is not declared in this package",
            location(directive),
            directive.source,
            directive.dependency
        );
    }
    let optional_values: BTreeSet<_> = matching
        .iter()
        .map(|dependency| dependency.optional)
        .collect();
    if optional_values.len() != 1 {
        bail!(
            "{}: dependency `{}` has conflicting optional settings across target-specific declarations",
            location(directive),
            directive.dependency
        );
    }

    let resolved_ids: BTreeSet<_> = root_node
        .deps
        .iter()
        .filter(|dependency| dependency.name == directive.dependency)
        .map(|dependency| dependency.pkg.clone())
        .collect();
    if resolved_ids.is_empty() {
        bail!(
            "{}: Cargo did not resolve dependency `{}`; check its target condition and manifest declaration",
            location(directive),
            directive.dependency
        );
    }
    if resolved_ids.len() > 1 {
        let packages = resolved_ids
            .iter()
            .filter_map(|id| packages.iter().find(|package| &package.id == id))
            .map(|package| format!("{} {}", package.name, package.version))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "{}: dependency key `{}` resolves to multiple packages ({packages}); forwarding is ambiguous across target-specific versions",
            location(directive),
            directive.dependency
        );
    }
    let package_id = resolved_ids.iter().next().expect("non-empty set");
    let package = packages
        .iter()
        .find(|package| &package.id == package_id)
        .with_context(|| {
            format!(
                "{}: Cargo resolved dependency `{}` but returned no package metadata",
                location(directive),
                directive.dependency
            )
        })?;

    Ok(DependencyFeatures {
        names: package.features.keys().cloned().collect(),
        optional: *optional_values.iter().next().expect("one optional value"),
    })
}

fn location(directive: &Directive) -> String {
    format!("line {}", directive.line)
}
