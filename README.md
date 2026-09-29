# project-auto-cleaner

Clean build and dependency directories from inactive Rust and JavaScript projects.

```sh
cargo install --path . --force
project-auto-cleaner /home/andrey/projects --days 2 --dry-run
```

Cleanup runs by default; use `--dry-run` to preview it. The default inactivity
threshold is 90 days. Only projects whose latest relevant file is strictly older
than the threshold are eligible. Source files are preserved.

The report groups cleanup results by project, with its name first, relative time
since the latest activity, its full path, and the size of each removed directory.
For example (sizes are illustrative):

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
```

Projects without separate cleanup directories are omitted from the detailed
report by default. Use `--verbose` to include them and show owners of shared
directories. Nested names include their relative parent path to distinguish
projects such as `one/frontend` and `two/frontend`.

Cleanup directories are Rust `target`, JavaScript `node_modules`, `.yarn/cache`,
and `.yarn/unplugged`. Generated output such as `.output`, `.nuxt`, and `.next`
is excluded from project discovery and activity checks. Dependency caches do
not count as source activity. Active or unverified owners protect shared
directories from deletion.

Space is estimated before removal and totaled only for successful removals
(or eligible directories in a dry run). Unix estimates use allocated disk blocks,
count hard-linked files once, and exclude files with links outside the directory
being removed. Other platforms use file lengths. Symbolic links are not followed.
Filesystem compression, snapshots, open files, and links between separate cleanup
directories can make the actual change in free disk space differ from the estimate.
Unreadable sizes are reported as unavailable rather than as zero.

Exit codes: `0` for success, `1` for scan/activity/deletion errors, and `2` for
invalid arguments or a root that cannot be scanned.
