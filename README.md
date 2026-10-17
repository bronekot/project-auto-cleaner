# Project auto cleaner

Safely remove dependency and build directories from inactive Rust and JavaScript projects.

```powershell
cargo install --path . --locked

# Show what would be deleted (the default; nothing is removed)
project-auto-cleaner E:\ --days 15
project-auto-cleaner E:\ --days 15 --verbose

# Actually delete
project-auto-cleaner E:\ --days 15 --delete
```

Nothing is deleted without `--delete`; deletion is permanent. A project is inactive when none of its source files changed for more than **90 days** (`--days N` changes this; an exact threshold match is still active). The report shows the disk space each directory occupies; a hard-linked file is counted once, and only if all of its links are inside the directory. Windows system folders (`$RECYCLE.BIN` and `System Volume Information` anywhere; `Program Files`, `Program Files (x86)`, `ProgramData`, `Recovery` and `Windows` at a volume root) are skipped automatically. Use `--exclude PATH` (relative to ROOT, repeatable) to skip other directories; excluded directories are not scanned at all. Run `--help` for all options.

Projects are identified by `Cargo.toml` and `package.json`. Cleanup targets and the contents they must have:

- `target` next to `Cargo.toml`, with Cargo's `CACHEDIR.TAG`;
- `node_modules`, with installed packages or a package-manager marker;
- `.yarn/cache`, with package archives, and `.yarn/unplugged`, with unplugged packages.

Source activity uses regular-file modification times. Dependency directories, version-control metadata and Windows system folders are ignored everywhere; generated output (`target`, `dist`, `build`, `coverage`, `.next`, `.output`, …) is ignored only directly inside a project root, so source folders with these names still count.

Shared directories are removed only when every owner is inactive. Cargo workspace members, in-workspace path dependencies and `package.workspace` pointers are owners; `workspace.exclude` follows Cargo's path-prefix rules. Every project nested inside a JavaScript project (including Cargo projects such as `src-tauri`) owns the ancestor's dependency directories, and the frontend of a Tauri app (a Cargo project with `tauri.conf.json`) owns its `target`.

Unreadable or unsupported workspace definitions (including member patterns containing `..`), unverified source links and Git-tracked files prevent deletion. An unreadable directory blocks only the targets whose owners it could affect. Symlinks and junctions are never followed or removed; a link inside a project that points outside it makes the project unverified, while links that resolve within the project (as in bun caches) are ignored. Projects without Git are cleaned normally; inside a Git repository, Git must work there. Custom Cargo target directories and custom package-manager cache locations are not cleaned.

With `--delete`, each target's owners are rescanned right before it is removed. The directory is then renamed aside and deleted, so on Windows a directory with files in use (a running build or dev server) is left intact. The checks are not an atomic filesystem snapshot, so avoid editing projects during cleanup.

Exit codes: `0` success, `1` something could not be verified or a deletion failed, `2` invalid arguments, ROOT or exclude.

```powershell
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```
