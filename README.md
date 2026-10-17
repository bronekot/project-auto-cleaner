# project-auto-cleaner

Safely remove build and dependency directories from inactive Rust and JavaScript projects.

```sh
cargo install --path . --locked

# Preview (the default; nothing is removed)
project-auto-cleaner E:\ --days 15
project-auto-cleaner /home/andrey/projects --days 2 --verbose

# Actually delete
project-auto-cleaner E:\ --days 15 --delete
```

Nothing is deleted without `--delete`; deletion is permanent and source files are preserved. The default inactivity threshold is 90 days (`--days N`); only projects whose latest relevant file is strictly older than the threshold are eligible. Use `--exclude PATH` (relative to ROOT, repeatable) to skip directories; excluded directories are not scanned at all. Run `--help` for all options.

The report groups cleanup results by project, with its name first, relative time since the latest activity, its full path, and the size of each directory. For example (sizes are illustrative):

```text
Project cleanup — dry run
Root: /home/andrey/projects
Clean artifacts after more than 2 days without activity.
Projects: 2 total · 1 inactive · 1 active · 0 unverified

EloRating — inactive, last activity 6 days 4 hours ago
  Path: /home/andrey/projects/EloRating
  Would remove: target (~1.42 GiB)

vps-calc — active, last activity 3 hours ago
  Path: /home/andrey/projects/vps-calc
  Kept: target
    Reason: vps-calc is active; last activity 3 hours ago

Result: 1 directory would be removed · 1 kept · 0 failed
Space to free: ~1.42 GiB (estimated)
Dry run: no files were deleted.
Run again with --delete to remove these directories.
```

Projects without separate cleanup directories are omitted from the detailed report by default. Use `--verbose` to include them, show all activity errors and show owners of shared directories. Nested names include their relative parent path to distinguish projects such as `one/frontend` and `two/frontend`.

## What is cleaned

Projects are identified by `Cargo.toml` and `package.json`. Cleanup directories and the contents they must have:

- `target` next to `Cargo.toml`, with Cargo's `CACHEDIR.TAG`;
- `node_modules`, with installed packages or a package-manager marker;
- `.yarn/cache`, with package archives, and `.yarn/unplugged`, with unplugged packages.

Source activity uses regular-file modification times. Dependency directories, version-control metadata and Windows system folders (`$RECYCLE.BIN` and `System Volume Information` anywhere; `Program Files`, `Program Files (x86)`, `ProgramData`, `Recovery` and `Windows` at a volume root) are ignored everywhere. Generated output (`target`, `dist`, `build`, `coverage`, `.next`, `.nuxt`, `.output`, …) is ignored only directly inside a project root, so source folders with these names still count.

## Safety

Shared directories are removed only when every owner is inactive. Cargo workspace members, in-workspace path dependencies and `package.workspace` pointers are owners; `workspace.exclude` follows Cargo's path-prefix rules. Every project nested inside a JavaScript project (including Cargo projects such as `src-tauri`) owns the ancestor's dependency directories, and the frontend of a Tauri app (a Cargo project with `tauri.conf.json`) owns its `target`. Active or unverified owners protect shared directories.

Unreadable or unsupported workspace definitions (including member patterns containing `..`), unverified source links and Git-tracked files prevent deletion. An unreadable directory blocks only the targets whose owners it could affect. Symlinks and junctions are never followed or removed; a link inside a project that points outside it makes the project unverified, while links that resolve within the project (as in bun caches) are ignored. Projects without Git are cleaned normally; inside a Git repository, Git must work there. Custom Cargo target directories and custom package-manager cache locations are not cleaned.

With `--delete`, each directory's owners are rescanned right before it is removed. The directory is then renamed aside and deleted, so on Windows a directory with files in use (a running build or dev server) is left intact. The checks are not an atomic filesystem snapshot, so avoid editing projects during cleanup.

## Space estimates

Space is estimated before removal and totaled only for successful removals (or eligible directories in a dry run). Estimates use allocated disk space, count hard-linked files once and exclude files with links outside the directory being removed; this works on Unix and on Windows. Symbolic links are not followed. Filesystem compression, snapshots, open files and links between separate cleanup directories can make the actual change in free disk space differ from the estimate. Unreadable sizes are reported as unavailable rather than as zero.

Exit codes: `0` for success, `1` for scan, activity, verification or deletion errors, and `2` for invalid arguments, ROOT or exclude.

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```
