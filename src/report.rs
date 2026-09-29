use project_auto_cleaner::{
    format_duration, format_relative_time, CleanupPlan, ProjectReport, ProjectStatus, RunReport,
    TargetOutcome, TargetReport,
};
use std::collections::BTreeMap;
use std::io::{self, Write};

pub fn write_report(
    out: &mut impl Write,
    plan: &CleanupPlan,
    report: &RunReport,
    dry_run: bool,
    verbose: bool,
) -> io::Result<()> {
    let mode = if dry_run { " — dry run" } else { "" };
    writeln!(out, "Project cleanup{mode}")?;
    writeln!(out, "Root: {}", plan.canonical_root.display())?;
    writeln!(
        out,
        "Clean artifacts after more than {} without activity.",
        format_duration(plan.threshold)
    )?;

    let mut fresh = 0;
    let mut stale = 0;
    let mut unverified = 0;
    for project in &report.projects {
        match project.status {
            ProjectStatus::Fresh { .. } => fresh += 1,
            ProjectStatus::Stale { .. } => stale += 1,
            ProjectStatus::Unverified { .. } => unverified += 1,
        }
    }
    writeln!(
        out,
        "Projects: {} total · {stale} inactive · {fresh} active · {unverified} unverified",
        report.projects.len()
    )?;

    // Show a shared directory once, under its closest owning project.
    let mut grouped = BTreeMap::<_, Vec<&TargetReport>>::new();
    let mut unassigned = Vec::new();
    for target in &report.targets {
        let owner = report
            .projects
            .iter()
            .filter(|project| {
                target.target.owners.contains(&project.project.id)
                    && target.target.path.starts_with(&project.project.root)
            })
            .max_by_key(|project| project.project.root.components().count());
        match owner {
            Some(owner) => grouped.entry(owner.project.id).or_default().push(target),
            None => unassigned.push(target),
        }
    }

    let mut hidden = 0;
    for project in &report.projects {
        let targets = grouped.remove(&project.project.id).unwrap_or_default();
        if !verbose
            && targets.is_empty()
            && !matches!(project.status, ProjectStatus::Unverified { .. })
        {
            hidden += 1;
            continue;
        }
        writeln!(out)?;
        let name = project.project.display_name(&plan.canonical_root);
        match &project.status {
            ProjectStatus::Fresh { latest_activity } | ProjectStatus::Stale { latest_activity } => {
                let status = if project.status.is_stale() {
                    "inactive"
                } else {
                    "active"
                };
                writeln!(
                    out,
                    "{name} — {status}, last activity {}",
                    format_relative_time(*latest_activity, plan.now)
                )?;
            }
            ProjectStatus::Unverified { .. } => {
                writeln!(out, "{name} — activity could not be verified")?;
            }
        }
        writeln!(out, "  Path: {}", project.project.root.display())?;
        if let ProjectStatus::Unverified { errors } = &project.status {
            for error in errors {
                writeln!(out, "  Error: {error}")?;
            }
        }
        if targets.is_empty() {
            let shared = report
                .targets
                .iter()
                .any(|target| target.target.owners.contains(&project.project.id));
            if shared {
                writeln!(
                    out,
                    "  Uses shared directories listed under their owning project."
                )?;
            } else {
                writeln!(out, "  No cleanup directories.")?;
            }
        }
        for target in targets {
            write_target(out, target, Some(project), plan, verbose)?;
        }
    }

    if !unassigned.is_empty() {
        writeln!(out, "\nDirectories without an identified project")?;
        for target in unassigned {
            write_target(out, target, None, plan, verbose)?;
        }
    }
    if hidden > 0 {
        writeln!(
            out,
            "\n{hidden} projects with no separate cleanup directories hidden; use --verbose to show all."
        )?;
    }
    if report.projects.is_empty() {
        writeln!(out, "\nNo Rust or JavaScript projects found.")?;
    } else if report.targets.is_empty() {
        writeln!(out, "\nNo cleanup directories found.")?;
    }

    // Target failures and activity errors are already printed with their project.
    if !plan.errors.is_empty() {
        writeln!(out, "\nScan errors: {}", plan.errors.len())?;
        for error in &plan.errors {
            writeln!(out, "  {error}")?;
        }
    }

    let mut removed = 0;
    let mut kept = 0;
    let mut failed = 0;
    let mut bytes = 0_u64;
    let mut unknown_sizes = 0;
    for target in &report.targets {
        match target.outcome {
            TargetOutcome::Deleted | TargetOutcome::WouldDelete => {
                removed += 1;
                match target.estimated_bytes {
                    Some(size) => bytes = bytes.saturating_add(size),
                    None => unknown_sizes += 1,
                }
            }
            TargetOutcome::Protected(_) => kept += 1,
            TargetOutcome::Failed(_) => failed += 1,
        }
    }
    let action = if dry_run {
        "would be removed"
    } else {
        "removed"
    };
    let noun = if removed == 1 {
        "directory"
    } else {
        "directories"
    };
    writeln!(
        out,
        "\nResult: {removed} {noun} {action} · {kept} kept · {failed} failed"
    )?;
    let space = if dry_run {
        "Space to free"
    } else {
        "Space freed"
    };
    if unknown_sizes > 0 && unknown_sizes == removed {
        writeln!(out, "{space}: size unavailable")?;
    } else if unknown_sizes > 0 {
        writeln!(out, "{space}: ~{} (known sizes only)", format_bytes(bytes))?;
    } else if removed == 0 {
        writeln!(out, "{space}: 0 B")?;
    } else {
        writeln!(out, "{space}: ~{} (estimated)", format_bytes(bytes))?;
    }
    if report.has_errors() {
        writeln!(out, "Completed with errors; see details above.")?;
    }
    if dry_run {
        writeln!(out, "Dry run: no files were deleted.")?;
    }
    Ok(())
}

fn write_target(
    out: &mut impl Write,
    target: &TargetReport,
    project: Option<&ProjectReport>,
    plan: &CleanupPlan,
    verbose: bool,
) -> io::Result<()> {
    let path = project
        .and_then(|project| target.target.path.strip_prefix(&project.project.root).ok())
        .unwrap_or(&target.target.path);
    let label = match target.outcome {
        TargetOutcome::WouldDelete => "Would remove",
        TargetOutcome::Deleted => "Removed",
        TargetOutcome::Protected(_) => "Kept",
        TargetOutcome::Failed(_) => "Failed to remove",
    };
    write!(out, "  {label}: {}", path.display())?;
    if matches!(
        target.outcome,
        TargetOutcome::WouldDelete | TargetOutcome::Deleted
    ) {
        match target.estimated_bytes {
            Some(bytes) => write!(out, " (~{})", format_bytes(bytes))?,
            None => write!(out, " (size unavailable)")?,
        }
    }
    writeln!(out)?;
    if let TargetOutcome::Protected(reason) | TargetOutcome::Failed(reason) = &target.outcome {
        writeln!(out, "    Reason: {reason}")?;
    }
    if verbose && target.target.owners.len() > 1 {
        let owners = target
            .target
            .owners
            .iter()
            .filter_map(|id| {
                plan.projects
                    .iter()
                    .find(|project| project.id == *id)
                    .map(|project| project.display_name(&plan.canonical_root))
            })
            .collect::<Vec<_>>();
        writeln!(out, "    Shared by: {}", owners.join(", "))?;
    }
    if verbose {
        if let Some(canonical) = &target.target.canonical_path {
            if canonical != &target.target.path {
                writeln!(out, "    Resolved path: {}", canonical.display())?;
            }
        }
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"] {
        value /= 1024.0;
        if value < 1024.0 || unit == "EiB" {
            return format!("{value:.2} {unit}");
        }
    }
    unreachable!()
}
