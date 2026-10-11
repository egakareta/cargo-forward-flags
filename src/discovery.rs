use crate::directive::Directive;
use crate::manifest::ParsedManifest;
use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::{Metadata, MetadataCommand, Package, PackageId};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_TEMP_PROJECT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone)]
pub struct DependencyFeatures {
    pub names: BTreeSet<String>,
    pub optional: bool,
}

pub fn workspace_manifests(manifest_path: &Path) -> Result<Vec<PathBuf>> {
    let mut command = MetadataCommand::new();
    command.manifest_path(manifest_path);
    command.no_deps();
    let metadata = command.exec().with_context(|| {
        format!(
            "could not inspect workspace for manifest `{}` using `cargo metadata --no-deps`",
            manifest_path.display()
        )
    })?;

    let mut manifests: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .map(|package| package.manifest_path.as_std_path().to_path_buf())
        .collect();
    manifests.sort();
    if manifests.is_empty() {
        bail!(
            "Cargo metadata returned no workspace packages for manifest `{}`",
            manifest_path.display()
        );
    }
    Ok(manifests)
}

pub fn discover(
    manifest_path: &Path,
    directives: &[Directive],
    source: &str,
    parsed: &ParsedManifest,
) -> Result<Vec<DependencyFeatures>> {
    let mut command = MetadataCommand::new();
    command.manifest_path(manifest_path);
    // Activating all features makes Cargo include optional dependency packages in the
    // resolved graph. We only read package metadata; no build is performed.
    command.features(cargo_metadata::CargoOpt::AllFeatures);
    let (metadata, isolated_root_id) = match command.exec() {
        Ok(metadata) => (metadata, None),
        Err(original_error) => {
            let (metadata, root_id) = isolated_metadata(manifest_path, source, parsed)
                .with_context(|| {
                    format!(
                        "could not resolve dependencies for manifest `{}` using `cargo metadata --all-features`; retrying without generated forwarding features also failed: {original_error:#}",
                        manifest_path.display()
                    )
                })?;
            (metadata, Some(root_id))
        }
    };

    let manifest = manifest_path.canonicalize().with_context(|| {
        format!(
            "could not resolve manifest path `{}`",
            manifest_path.display()
        )
    })?;
    let root_package = match isolated_root_id {
        Some(root_id) => metadata
            .packages
            .iter()
            .find(|package| package.id == root_id),
        None => metadata
            .packages
            .iter()
            .find(|package| package.manifest_path.as_std_path() == manifest),
    }
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

/// Cargo validates every feature reference in a package before it can return resolved metadata.
/// That prevents us from pruning an old generated feature which names a dependency feature that
/// has since been removed. Retry resolution from an isolated copy of the package manifest with
/// only the managed feature contents removed. The user's manifest and lockfile remain untouched.
fn isolated_metadata(
    manifest_path: &Path,
    source: &str,
    parsed: &ParsedManifest,
) -> Result<(Metadata, PackageId)> {
    let mut no_deps = MetadataCommand::new();
    no_deps.manifest_path(manifest_path);
    no_deps.no_deps();
    let workspace_metadata = no_deps.exec().with_context(|| {
        format!(
            "could not inspect manifest `{}` without resolving dependencies",
            manifest_path.display()
        )
    })?;

    let manifest = manifest_path.canonicalize().with_context(|| {
        format!(
            "could not resolve manifest path `{}`",
            manifest_path.display()
        )
    })?;
    let root_package = workspace_metadata
        .packages
        .iter()
        .find(|package| package.manifest_path.as_std_path() == manifest)
        .with_context(|| {
            format!(
                "Cargo metadata did not return package manifest `{}` (returned: {})",
                manifest.display(),
                workspace_metadata
                    .packages
                    .iter()
                    .map(|package| package.manifest_path.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let workspace_root = workspace_metadata.workspace_root.as_std_path();
    let workspace_manifest_path = workspace_root.join("Cargo.toml");
    let workspace_manifest = read_toml(&workspace_manifest_path)?;
    let cleaned_source =
        crate::manifest::rewrite(source, parsed, &vec![String::new(); parsed.blocks.len()])?;
    let cleaned_package_manifest = read_toml_value(&cleaned_source)?;
    let workspace_dependencies = workspace_manifest
        .get("workspace")
        .and_then(|table| table.get("dependencies"));

    let mut isolated = toml::map::Map::new();
    let mut isolated_workspace = toml::map::Map::new();
    if let Some(resolver) = workspace_manifest
        .get("workspace")
        .and_then(|table| table.get("resolver"))
    {
        isolated_workspace.insert("resolver".to_owned(), resolver.clone());
    }
    isolated.insert(
        "workspace".to_owned(),
        toml::Value::Table(isolated_workspace),
    );

    let mut package = toml::map::Map::new();
    package.insert("name".to_owned(), root_package.name.to_string().into());
    package.insert(
        "version".to_owned(),
        root_package.version.to_string().into(),
    );
    package.insert(
        "edition".to_owned(),
        root_package.edition.to_string().into(),
    );
    if let Some(rust_version) = &root_package.rust_version {
        package.insert("rust-version".to_owned(), rust_version.to_string().into());
    }
    isolated.insert("package".to_owned(), toml::Value::Table(package));

    if let Some(features) = cleaned_package_manifest.get("features") {
        isolated.insert("features".to_owned(), features.clone());
    }
    let package_dir = manifest_path
        .parent()
        .context("manifest path has no parent directory")?;
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(dependencies) = cleaned_package_manifest.get(section) {
            isolated.insert(
                section.to_owned(),
                resolve_dependency_table(
                    dependencies,
                    workspace_dependencies,
                    package_dir,
                    workspace_root,
                )?,
            );
        }
    }
    if let Some(targets) = cleaned_package_manifest.get("target") {
        isolated.insert(
            "target".to_owned(),
            resolve_target_dependencies(
                targets,
                workspace_dependencies,
                package_dir,
                workspace_root,
            )?,
        );
    }

    // Workspace patches affect the version/source Cargo resolves, so retain them in the isolated
    // package and make their relative paths independent of the temporary directory.
    if let Some(value) = workspace_manifest.get("patch") {
        isolated.insert(
            "patch".to_owned(),
            absolutize_patch_paths(value, workspace_root)?,
        );
    }
    if let Some(value) = workspace_manifest.get("replace") {
        isolated.insert(
            "replace".to_owned(),
            absolutize_replace_paths(value, workspace_root)?,
        );
    }

    let temporary = TemporaryProject::create()?;
    let manifest_text = toml::to_string(&toml::Value::Table(isolated))
        .context("could not serialize isolated discovery manifest")?;
    fs::write(temporary.0.join("Cargo.toml"), manifest_text)
        .context("could not write isolated discovery manifest")?;
    fs::create_dir(temporary.0.join("src"))
        .context("could not create isolated discovery source directory")?;
    fs::write(temporary.0.join("src/lib.rs"), "")
        .context("could not write isolated discovery source")?;

    let lockfile = workspace_root.join("Cargo.lock");
    if lockfile.is_file() {
        fs::copy(lockfile, temporary.0.join("Cargo.lock"))
            .context("could not copy workspace lockfile for isolated dependency discovery")?;
    }

    let mut command = MetadataCommand::new();
    command
        .manifest_path(temporary.0.join("Cargo.toml"))
        .current_dir(workspace_root)
        .features(cargo_metadata::CargoOpt::AllFeatures);
    let metadata = command.exec().context("isolated cargo metadata failed")?;
    let [root_id] = metadata.workspace_members.as_slice() else {
        bail!(
            "isolated Cargo metadata returned {} workspace packages, expected one",
            metadata.workspace_members.len()
        );
    };
    let root_id = root_id.clone();
    if !metadata
        .packages
        .iter()
        .any(|package| package.id == root_id)
    {
        bail!("isolated Cargo metadata did not return its root package");
    }
    Ok((metadata, root_id))
}

fn read_toml(path: &Path) -> Result<toml::Value> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("could not read manifest `{}`", path.display()))?;
    read_toml_value(&source)
        .with_context(|| format!("could not parse manifest `{}`", path.display()))
}

fn read_toml_value(source: &str) -> Result<toml::Value> {
    toml::from_str(source).context("manifest is not valid TOML")
}

fn resolve_target_dependencies(
    targets: &toml::Value,
    workspace_dependencies: Option<&toml::Value>,
    package_dir: &Path,
    workspace_root: &Path,
) -> Result<toml::Value> {
    let Some(targets) = targets.as_table() else {
        return Ok(targets.clone());
    };
    let mut resolved_targets = toml::map::Map::new();
    for (target, target_value) in targets {
        let Some(target_table) = target_value.as_table() else {
            resolved_targets.insert(target.clone(), target_value.clone());
            continue;
        };
        let mut resolved_target = toml::map::Map::new();
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(dependencies) = target_table.get(section) {
                resolved_target.insert(
                    section.to_owned(),
                    resolve_dependency_table(
                        dependencies,
                        workspace_dependencies,
                        package_dir,
                        workspace_root,
                    )?,
                );
            }
        }
        resolved_targets.insert(target.clone(), toml::Value::Table(resolved_target));
    }
    Ok(toml::Value::Table(resolved_targets))
}

fn resolve_dependency_table(
    dependencies: &toml::Value,
    workspace_dependencies: Option<&toml::Value>,
    package_dir: &Path,
    workspace_root: &Path,
) -> Result<toml::Value> {
    let Some(dependencies) = dependencies.as_table() else {
        return Ok(dependencies.clone());
    };
    let mut resolved = toml::map::Map::new();
    for (name, dependency) in dependencies {
        let uses_workspace = dependency
            .get("workspace")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let member_overrides_path = dependency
            .as_table()
            .is_some_and(|table| table.contains_key("path"));
        let dependency_path_base = if uses_workspace && !member_overrides_path {
            workspace_root
        } else {
            package_dir
        };
        let mut value = if uses_workspace {
            let inherited = workspace_dependencies
                .and_then(|dependencies| dependencies.get(name))
                .with_context(|| {
                    format!("dependency `{name}` inherits from a missing workspace dependency")
                })?;
            merge_workspace_dependency(inherited, dependency)?
        } else {
            dependency.clone()
        };
        if let Some(table) = value.as_table_mut() {
            table.remove("workspace");
            absolutize_path(table, dependency_path_base);
        }
        resolved.insert(name.clone(), value);
    }
    Ok(toml::Value::Table(resolved))
}

fn merge_workspace_dependency(
    inherited: &toml::Value,
    member: &toml::Value,
) -> Result<toml::Value> {
    let mut inherited = match inherited {
        toml::Value::String(version) => {
            let mut table = toml::map::Map::new();
            table.insert("version".to_owned(), version.clone().into());
            toml::Value::Table(table)
        }
        toml::Value::Table(table) => toml::Value::Table(table.clone()),
        _ => bail!("workspace dependency must be a version string or table"),
    };
    let Some(member) = member.as_table() else {
        return Ok(inherited);
    };
    let Some(inherited) = inherited.as_table_mut() else {
        unreachable!("workspace dependency was normalized to a table")
    };
    for (key, value) in member {
        if key == "workspace" {
            continue;
        }
        if key == "features" {
            let additions = value
                .as_array()
                .context("dependency features must be an array")?;
            let features = inherited
                .entry("features".to_owned())
                .or_insert_with(|| toml::Value::Array(Vec::new()))
                .as_array_mut()
                .context("workspace dependency features must be an array")?;
            for feature in additions {
                if !features.contains(feature) {
                    features.push(feature.clone());
                }
            }
        } else {
            inherited.insert(key.clone(), value.clone());
        }
    }
    Ok(toml::Value::Table(inherited.clone()))
}

fn absolutize_patch_paths(value: &toml::Value, workspace_root: &Path) -> Result<toml::Value> {
    let Some(tables) = value.as_table() else {
        return Ok(value.clone());
    };
    let mut result = toml::map::Map::new();
    for (source, patch) in tables {
        result.insert(
            source.clone(),
            resolve_dependency_table(patch, None, workspace_root, workspace_root)?,
        );
    }
    Ok(toml::Value::Table(result))
}

fn absolutize_replace_paths(value: &toml::Value, workspace_root: &Path) -> Result<toml::Value> {
    let Some(replacements) = value.as_table() else {
        return Ok(value.clone());
    };
    let mut result = replacements.clone();
    for (_, replacement) in result.iter_mut() {
        if let Some(spec) = replacement.as_table_mut() {
            absolutize_path(spec, workspace_root);
        }
    }
    Ok(toml::Value::Table(result))
}

fn absolutize_path(table: &mut toml::map::Map<String, toml::Value>, base: &Path) {
    if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
        let path = Path::new(path);
        if path.is_relative() {
            table.insert(
                "path".to_owned(),
                base.join(path).to_string_lossy().into_owned().into(),
            );
        }
    }
}

struct TemporaryProject(PathBuf);

impl TemporaryProject {
    fn create() -> Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..100 {
            let id = NEXT_TEMP_PROJECT.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "cargo-forward-flags-discovery-{}-{id}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("could not create temporary directory `{}`", path.display())
                    });
                }
            }
        }
        Err(anyhow!(
            "could not allocate a temporary discovery directory"
        ))
    }
}

impl Drop for TemporaryProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
