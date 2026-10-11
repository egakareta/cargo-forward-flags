use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cargo-forward-flags-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, relative: impl AsRef<Path>, contents: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn manifest(&self) -> PathBuf {
        self.0.join("app/Cargo.toml")
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cargo-forward-flags"))
            .args(args)
            .arg("--manifest-path")
            .arg(self.manifest())
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn add_feature_dependencies(fixture: &Fixture) {
    fixture.write(
        "feature-source/Cargo.toml",
        "[package]\nname = \"feature-source\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\ndefault = [\"std\"]\nalloc = []\nrc = []\nstd = [\"alloc\"]\nunstable = []\n",
    );
    fixture.write("feature-source/src/lib.rs", "pub fn value() {}\n");
    fixture.write(
        "empty-source/Cargo.toml",
        "[package]\nname = \"empty-source\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    fixture.write("empty-source/src/lib.rs", "pub fn value() {}\n");
}

fn app_manifest(dependencies: &str, features: &str) -> String {
    format!(
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{dependencies}\n[features]\nmanual = []\n{features}"
    )
}

#[test]
fn forwards_renamed_optional_path_dependency_and_handles_empty_feature_sets() {
    let fixture = Fixture::new();
    add_feature_dependencies(&fixture);
    let original = app_manifest(
        "alias = { package = \"feature-source\", path = \"../feature-source\", optional = true }\nempty = { package = \"empty-source\", path = \"../empty-source\" }\n",
        "# <forward alias prefix=\"alias-\" exclude=\"rc\">\n# </forward>\n# <forward empty>\n# </forward>\n",
    );
    fixture.write("app/Cargo.toml", &original);
    fixture.write("app/src/main.rs", "fn main() {}\n");

    let output = fixture.command(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let updated = fs::read_to_string(fixture.manifest()).unwrap();
    assert!(updated.contains("alias-alloc = [\"alias?/alloc\"]\n"));
    assert!(updated.contains("alias-std = [\"alias?/std\"]\n"));
    assert!(updated.contains("alias-unstable = [\"alias?/unstable\"]\n"));
    assert!(!updated.contains("alias-default"));
    assert!(!updated.contains("alias-rc"));
    assert!(updated.contains("manual = []\n"));
    assert!(updated.contains("# <forward empty>\n# </forward>"));

    let second = fixture.command(&[]);
    assert!(second.status.success());
    assert_eq!(updated, fs::read_to_string(fixture.manifest()).unwrap());

    let check = fixture.command(&["--check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn workspace_inherited_dependency_is_discovered() {
    let fixture = Fixture::new();
    fixture.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"feature-source\"]\nresolver = \"3\"\n\n[workspace.dependencies]\nalias = { package = \"feature-source\", path = \"feature-source\" }\n",
    );
    fixture.write(
        "feature-source/Cargo.toml",
        "[package]\nname = \"feature-source\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nalpha = []\nbeta = []\n",
    );
    fixture.write("feature-source/src/lib.rs", "pub fn value() {}\n");
    fixture.write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nalias = { workspace = true }\n\n[features]\n# <forward alias>\n# </forward>\n",
    );
    fixture.write("app/src/main.rs", "fn main() {}\n");

    let output = fixture.command(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let updated = fs::read_to_string(fixture.manifest()).unwrap();
    assert!(updated.contains("alpha = [\"alias/alpha\"]"));
    assert!(updated.contains("beta = [\"alias/beta\"]"));
}

#[test]
fn check_and_dry_run_do_not_modify_and_collisions_are_errors() {
    let fixture = Fixture::new();
    add_feature_dependencies(&fixture);
    fixture.write(
        "app/Cargo.toml",
        &app_manifest(
            "dep = { package = \"feature-source\", path = \"../feature-source\" }\n",
            "# <forward dep>\n# </forward>\n",
        ),
    );
    fixture.write("app/src/main.rs", "fn main() {}\n");
    let initial = fs::read_to_string(fixture.manifest()).unwrap();

    let check = fixture.command(&["--check"]);
    assert!(!check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("+alloc"));
    assert_eq!(initial, fs::read_to_string(fixture.manifest()).unwrap());

    let dry_run = fixture.command(&["--dry-run"]);
    assert!(dry_run.status.success());
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("+alloc"));
    assert_eq!(initial, fs::read_to_string(fixture.manifest()).unwrap());

    fixture.write(
        "app/Cargo.toml",
        &app_manifest(
            "dep = { package = \"feature-source\", path = \"../feature-source\" }\n",
            "alloc = []\n# <forward dep>\n# </forward>\n",
        ),
    );
    let collision = fixture.command(&[]);
    assert!(!collision.status.success());
    assert!(String::from_utf8_lossy(&collision.stderr).contains("collides"));
}

#[test]
fn manifest_without_features_table_is_a_noop() {
    let fixture = Fixture::new();
    let original = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nmissing = { path = \"../missing\" }\n";
    fixture.write("app/Cargo.toml", original);

    let output = fixture.command(&[]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(original, fs::read_to_string(fixture.manifest()).unwrap());
}

#[test]
fn preserves_crlf_and_supports_cargo_subcommand_argv_convention() {
    let fixture = Fixture::new();
    add_feature_dependencies(&fixture);
    let manifest = app_manifest(
        "dep = { package = \"feature-source\", path = \"../feature-source\" }\n",
        "# <forward dep>\n# </forward>\n",
    )
    .replace('\n', "\r\n");
    fixture.write("app/Cargo.toml", &manifest);
    fixture.write("app/src/main.rs", "fn main() {}\n");
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-forward-flags"))
        .arg("forward-flags")
        .arg("--manifest-path")
        .arg(fixture.manifest())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let updated = fs::read(fixture.manifest()).unwrap();
    assert!(updated.windows(2).any(|pair| pair == b"\r\n"));
    assert!(!updated.windows(2).any(|pair| pair == b"\n\n"));
}

#[test]
fn clap_help_documents_directives_and_check_modes_are_exclusive() {
    let help = Command::new(env!("CARGO_BIN_EXE_cargo-forward-flags"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--manifest-path"));
    assert!(help.contains("# <forward dependency"));
    assert!(help.contains("default feature is always omitted"));

    let conflict = Command::new(env!("CARGO_BIN_EXE_cargo-forward-flags"))
        .args(["--check", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(conflict.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("cannot be used with"));
}
