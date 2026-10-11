use crate::{discovery, generate, manifest};
use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(
    name = "cargo-forward-flags",
    version,
    about = "Generate Cargo feature forwarding declarations",
    long_about = "Discovers dependency features with Cargo metadata and maintains only the text between forwarding markers in Cargo.toml. Place blocks in the top-level [features] table.\n\nSyntax:\n  # <forward dependency>\n  # <forward dependency prefix=\"foo-\">\n  # <forward dependency exclude=\"std,alloc\">\n  # <forward dependency prefix=\"foo-\" exclude=\"std,alloc\">\n  # </forward>\n\nThe dependency is its manifest key, including renamed dependencies. Attribute order is flexible. The default feature is always omitted. Optional dependencies use conditional dependency?/feature forwarding.",
    after_help = "Use `cargo forward-flags` or invoke `cargo-forward-flags` directly."
)]
struct Cli {
    /// Operate on this Cargo.toml (defaults to ./Cargo.toml)
    #[arg(long, value_name = "PATH")]
    manifest_path: Option<PathBuf>,

    /// Process every package manifest in the selected workspace
    #[arg(long)]
    workspace: bool,

    /// Exit unsuccessfully if generated content is outdated; do not write
    #[arg(long, conflicts_with = "dry_run")]
    check: bool,

    /// Print diffs without modifying any manifests
    #[arg(long)]
    dry_run: bool,
}

pub fn run() -> Result<()> {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    // Cargo's external-subcommand convention may include the subcommand name as argv[1].
    if args.first().is_some_and(|arg| arg == "forward-flags") {
        args.remove(0);
    }
    let cli = Cli::parse_from(std::iter::once(OsString::from("cargo-forward-flags")).chain(args));

    let manifest_path = cli
        .manifest_path
        .unwrap_or_else(|| PathBuf::from("Cargo.toml"));
    let manifest_path = fs::canonicalize(&manifest_path)
        .with_context(|| format!("could not find manifest `{}`", manifest_path.display()))?;
    let mut workspace = if cli.workspace {
        Some(discovery::Workspace::new(&manifest_path)?)
    } else {
        None
    };
    let manifest_paths = workspace
        .as_ref()
        .map(|workspace| workspace.manifests().to_vec())
        .unwrap_or_else(|| vec![manifest_path]);

    let mut prepared = Vec::new();
    let mut failures = Vec::new();
    for path in &manifest_paths {
        match prepare_manifest(path, workspace.as_mut()) {
            Ok(Some(update)) => prepared.push(update),
            Ok(None) => {}
            Err(error) => failures.push((path.clone(), error)),
        }
    }
    if !failures.is_empty() {
        if !cli.workspace {
            return Err(failures
                .into_iter()
                .next()
                .expect("a failure was recorded")
                .1);
        }
        bail!(
            "could not process all workspace manifests:\n{}",
            failures
                .iter()
                .map(|(path, error)| format!("  - {}: {error:#}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    let mut outdated = Vec::new();
    let mut changed = false;
    for update in &prepared {
        if update.updated == update.original {
            continue;
        }
        changed = true;
        if cli.check || cli.dry_run {
            print!(
                "{}",
                manifest::diff(
                    &update.path,
                    &update.parsed,
                    &update.generated,
                    &update.original
                )
            );
        }
        if cli.check {
            outdated.push(update.path.display().to_string());
        }
    }

    if cli.check && !outdated.is_empty() {
        bail!(
            "generated forwarding content is outdated in {}{}; run `cargo forward-flags` to update it",
            if cli.workspace {
                "workspace manifest(s): "
            } else {
                ""
            },
            outdated.join(", ")
        );
    }
    if cli.dry_run {
        if !changed {
            println!("No generated features need updating.");
        }
        return Ok(());
    }
    if cli.check {
        return Ok(());
    }

    for update in prepared {
        if update.updated != update.original {
            manifest::write_atomically(&update.path, &update.original, &update.updated)?;
            eprintln!("Updated {}", update.path.display());
        }
    }
    Ok(())
}

struct PreparedManifest {
    path: PathBuf,
    original: String,
    parsed: manifest::ParsedManifest,
    generated: Vec<String>,
    updated: String,
}

fn prepare_manifest(
    path: &Path,
    workspace: Option<&mut discovery::Workspace>,
) -> Result<Option<PreparedManifest>> {
    let original = fs::read_to_string(path)
        .with_context(|| format!("could not read manifest `{}`", path.display()))?;
    let parsed = manifest::parse_manifest(&original).with_context(|| {
        format!(
            "could not parse forwarding directives in `{}`",
            path.display()
        )
    })?;
    if !parsed.has_features_table || parsed.blocks.is_empty() {
        return Ok(None);
    }
    let directives: Vec<_> = parsed
        .blocks
        .iter()
        .map(|block| block.directive.clone())
        .collect();
    let dependencies = match workspace {
        Some(workspace) => workspace.discover(path, &directives, &original, &parsed)?,
        None => discovery::discover(path, &directives, &original, &parsed)?,
    };
    let generated = generate::generate(&directives, &dependencies, &parsed.manual_features)?;
    let updated = manifest::rewrite(&original, &parsed, &generated)?;

    Ok(Some(PreparedManifest {
        path: path.to_path_buf(),
        original,
        parsed,
        generated,
        updated,
    }))
}
