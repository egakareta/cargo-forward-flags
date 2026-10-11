use crate::{discovery, generate, manifest};
use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{env, ffi::OsString, fs, path::PathBuf};

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

    /// Exit unsuccessfully if generated content is outdated; do not write
    #[arg(long, conflicts_with = "dry_run")]
    check: bool,

    /// Print a diff without modifying the manifest
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
    let original = fs::read_to_string(&manifest_path)
        .with_context(|| format!("could not read manifest `{}`", manifest_path.display()))?;
    let parsed = manifest::parse_manifest(&original).with_context(|| {
        format!(
            "could not parse forwarding directives in `{}`",
            manifest_path.display()
        )
    })?;
    if !parsed.has_features_table {
        return Ok(());
    }
    let directives: Vec<_> = parsed
        .blocks
        .iter()
        .map(|block| block.directive.clone())
        .collect();
    let dependencies = discovery::discover(&manifest_path, &directives)?;
    let generated = generate::generate(&directives, &dependencies, &parsed.manual_features)?;
    let updated = manifest::rewrite(&original, &parsed, &generated)?;

    if updated == original {
        if cli.dry_run {
            println!("No generated features need updating.");
        }
        return Ok(());
    }
    if cli.check {
        print!(
            "{}",
            manifest::diff(&manifest_path, &parsed, &generated, &original)
        );
        bail!(
            "generated forwarding content is outdated in `{}`; run `cargo forward-flags` to update it",
            manifest_path.display()
        );
    }
    if cli.dry_run {
        print!(
            "{}",
            manifest::diff(&manifest_path, &parsed, &generated, &original)
        );
        return Ok(());
    }

    manifest::write_atomically(&manifest_path, &original, &updated)?;
    eprintln!("Updated {}", manifest_path.display());
    Ok(())
}
