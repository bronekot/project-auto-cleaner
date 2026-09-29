use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "project-auto-cleaner-cli-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn project(&self, name: &str, old: bool, artifacts: bool) {
        let project = self.0.join(name);
        fs::create_dir_all(&project).unwrap();
        let manifest = project.join("package.json");
        fs::write(&manifest, "{}").unwrap();
        if old {
            File::options()
                .write(true)
                .open(&manifest)
                .unwrap()
                .set_times(
                    FileTimes::new()
                        .set_modified(SystemTime::now() - Duration::from_secs(10 * 86_400)),
                )
                .unwrap();
        }
        if artifacts {
            fs::create_dir_all(project.join("node_modules")).unwrap();
            fs::write(
                project.join("node_modules/dependency.js"),
                "x".repeat(8_192),
            )
            .unwrap();
        }
    }

    fn run(&self, root: &Path, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_project-auto-cleaner"))
            .arg(root)
            .args(["--days", "2"])
            .args(extra)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn preview_and_cleanup_report_space_and_preserve_active_projects() {
    let fixture = Fixture::new();
    fixture.project("old app", true, true);
    fixture.project("active-app", false, true);
    fixture.project("empty-app", true, false);

    let preview = stdout(&fixture.run(&fixture.0, &["--dry-run"]));
    assert!(preview.contains("Projects: 3 total · 2 inactive · 1 active · 0 unverified"));
    assert!(preview.contains("old app — inactive, last activity 10 days ago\n  Path:"));
    assert!(preview.contains("Would remove: node_modules (~"));
    assert!(preview.contains("active-app — active"));
    assert!(preview.contains("Reason: active-app is active; last activity"));
    assert!(preview.contains("1 directory would be removed · 1 kept · 0 failed"));
    assert!(preview.contains("Space to free: ~"));
    assert!(preview.contains("Dry run: no files were deleted."));
    assert!(!preview.contains("empty-app"));
    assert!(!preview.contains("UNIX_EPOCH"));
    assert!(!preview.contains("project #"));
    assert!(fixture
        .0
        .join("old app/node_modules/dependency.js")
        .exists());

    let verbose = stdout(&fixture.run(&fixture.0, &["--dry-run", "--verbose"]));
    assert!(verbose.contains("empty-app — inactive"));
    assert!(verbose.contains("No cleanup directories."));

    let cleaned = stdout(&fixture.run(&fixture.0, &[]));
    assert!(cleaned.contains("Removed: node_modules (~"));
    assert!(cleaned.contains("1 directory removed · 1 kept · 0 failed"));
    let preview_size = preview
        .lines()
        .find_map(|line| line.strip_prefix("Space to free: "))
        .unwrap();
    let freed_size = cleaned
        .lines()
        .find_map(|line| line.strip_prefix("Space freed: "))
        .unwrap();
    assert_eq!(freed_size, preview_size);
    assert!(!fixture.0.join("old app/node_modules").exists());
    assert!(fixture.0.join("old app/package.json").exists());
    assert!(fixture
        .0
        .join("active-app/node_modules/dependency.js")
        .exists());
}

#[test]
fn nested_names_are_distinguishable_and_scanning_a_project_itself_has_a_name() {
    let fixture = Fixture::new();
    fixture.project("one/frontend", true, true);
    fixture.project("two/frontend", true, true);
    let output = stdout(&fixture.run(&fixture.0, &["--dry-run"]));
    assert!(output.contains("one/frontend — inactive"));
    assert!(output.contains("two/frontend — inactive"));

    let output = stdout(&fixture.run(&fixture.0.join("one/frontend"), &["--dry-run"]));
    assert!(output.contains("\nfrontend — inactive"));
}

#[test]
fn empty_scan_does_not_claim_to_free_space() {
    let fixture = Fixture::new();
    let output = stdout(&fixture.run(&fixture.0, &["--dry-run"]));
    assert!(output.contains("No Rust or JavaScript projects found."));
    assert!(output.contains("0 directories would be removed"));
    assert!(output.contains("Space to free: 0 B"));
}
