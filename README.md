# cargo-forward-flags

`cargo-forward-flags` discovers the features of resolved Cargo dependencies and
maintains feature-forwarding declarations in `Cargo.toml`.

## Install

```sh
cargo install cargo-forward-flags
cargo forward-flags
```

Options:

- `--manifest-path <path>` selects a manifest (defaults to `Cargo.toml` in the
  current directory).
- `--workspace` processes every package manifest in the selected Cargo
  workspace.
- `--check` does not write and exits unsuccessfully if generated content is
  outdated. It cannot be combined with `--dry-run`.
- `--dry-run` prints the changes without writing any manifests.
- `--help` prints command help.

## Usage

Place one or more forwarding blocks in the top-level `[features]` table:

```toml
[dependencies]
json = { package = "serde", version = "1" }

[features]
json-alloc = [] # manually managed declarations remain untouched

# <forward json prefix="json-">
# </forward>
```

Supported forms:

```toml
# <forward dependency>
# <forward dependency prefix="foo-">
# <forward dependency exclude="std,alloc,rc">
# <forward dependency prefix="foo-" exclude="std,rc">
# </forward>
```
