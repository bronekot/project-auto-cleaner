use project_auto_cleaner::{
    build_cleanup_plan, display_path, execute_plan, format_age, format_bytes, parse_cli,
    strip_verbatim, threshold_for_days, usage, CliOptions, ProjectStatus, RunReport, ScanOptions,
    TargetOutcome,
};
use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;

/// Aligns detail lines under the path column of status lines.
const INDENT: &str = "                ";
/// Activity errors shown per project without --verbose.
const SHOWN_ERRORS: usize = 3;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let options = parse_cli(std::env::args_os())
        .map_err(|error| format!("{error}\n\nRun 'project-auto-cleaner --help' for usage."))?;
    if options.help {
        println!("{}", usage());
        return Ok(ExitCode::SUCCESS);
    }
    if options.version {
        println!("project-auto-cleaner {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    let scan = ScanOptions {
        now: SystemTime::now(),
        threshold: threshold_for_days(options.days).map_err(|error| error.to_string())?,
        excludes: options.excludes.clone(),
    };
    let plan = build_cleanup_plan(&options.root, &scan).map_err(|error| error.to_string())?;
    let report = execute_plan(&plan, !options.delete);
    print_report(&report, scan.now, &options);

    if report.has_errors() {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn print_report(report: &RunReport, now: SystemTime, options: &CliOptions) {
    let shown = |path: &Path| display_path(&report.root, path);
    println!("Root: {}", strip_verbatim(&report.root).display());
    println!(
        "Projects: {} (inactive after {} days)",
        report.projects.len(),
        options.days
    );
    for project in &report.projects {
        let kinds = project
            .project
            .markers
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("+");
        let root = shown(&project.project.root);
        match &project.status {
            ProjectStatus::Fresh { latest_activity } => {
                if options.verbose {
                    println!(
                        "  {:<14}{root} [{kinds}], changed {}",
                        "ACTIVE",
                        format_age(*latest_activity, now)
                    );
                }
            }
            ProjectStatus::Stale { latest_activity } => println!(
                "  {:<14}{root} [{kinds}], changed {}",
                "INACTIVE",
                format_age(*latest_activity, now)
            ),
            ProjectStatus::Unverified { errors } => {
                println!(
                    "  {:<14}{root} [{kinds}], {} error(s)",
                    "UNVERIFIED",
                    errors.len()
                );
                let limit = if options.verbose {
                    errors.len()
                } else {
                    SHOWN_ERRORS
                };
                for error in errors.iter().take(limit) {
                    println!("{INDENT}{}: {}", shown(&error.path), error.message);
                }
                if errors.len() > limit {
                    println!(
                        "{INDENT}... and {} more (use --verbose)",
                        errors.len() - limit
                    );
                }
            }
        }
    }

    println!("Targets: {}", report.targets.len());
    for target in &report.targets {
        let (label, size) = match &target.outcome {
            TargetOutcome::WouldDelete => ("WOULD DELETE", true),
            TargetOutcome::Deleted => ("DELETED", true),
            TargetOutcome::Protected(_) if !options.verbose => continue,
            TargetOutcome::Protected(_) => ("PROTECTED", false),
            TargetOutcome::Unverified(_) => ("UNVERIFIED", false),
            TargetOutcome::Failed(_) => ("FAILED", false),
        };
        let size = if size {
            format!(" ({})", format_bytes(target.bytes))
        } else {
            String::new()
        };
        println!(
            "  {label:<14}{:<16} {}{size}",
            target.target.kind,
            shown(&target.target.path)
        );
        match &target.outcome {
            TargetOutcome::Protected(reason)
            | TargetOutcome::Unverified(reason)
            | TargetOutcome::Failed(reason) => println!("{INDENT}{reason}"),
            TargetOutcome::WouldDelete | TargetOutcome::Deleted => {}
        }
        if options.verbose {
            let owners = target
                .target
                .owners
                .iter()
                .filter_map(|id| {
                    report
                        .projects
                        .iter()
                        .find(|project| project.project.id == *id)
                })
                .map(|project| shown(&project.project.root))
                .collect::<Vec<_>>()
                .join(", ");
            println!("{INDENT}owners: {owners}");
        }
    }

    let count = |predicate: fn(&TargetOutcome) -> bool| {
        report
            .targets
            .iter()
            .filter(|target| predicate(&target.outcome))
            .count()
    };
    let freed = report
        .targets
        .iter()
        .filter(|target| {
            matches!(
                target.outcome,
                TargetOutcome::WouldDelete | TargetOutcome::Deleted
            )
        })
        .map(|target| target.bytes)
        .sum::<u64>();
    let protected = count(|outcome| matches!(outcome, TargetOutcome::Protected(_)));
    let others = format!(
        "{protected} protected, {} unverified, {} failed",
        count(|outcome| matches!(outcome, TargetOutcome::Unverified(_))),
        count(|outcome| matches!(outcome, TargetOutcome::Failed(_))),
    );
    if options.delete {
        println!(
            "Summary: {} deleted, freed {}; {others}",
            count(|outcome| matches!(outcome, TargetOutcome::Deleted)),
            format_bytes(freed)
        );
    } else {
        println!(
            "Summary: {} would be deleted, freeing {}; {others}",
            count(|outcome| matches!(outcome, TargetOutcome::WouldDelete)),
            format_bytes(freed)
        );
    }
    let has_active = report
        .projects
        .iter()
        .any(|project| matches!(project.status, ProjectStatus::Fresh { .. }));
    if !options.verbose && (protected > 0 || has_active) {
        println!("Use --verbose to list protected targets and active projects.");
    }

    if !report.errors.is_empty() {
        println!("Errors: {}", report.errors.len());
        for error in &report.errors {
            match &error.path {
                Some(path) => println!("  {}: {}", shown(path), error.message),
                None => println!("  {}", error.message),
            }
        }
    }

    if !options.delete && freed > 0 {
        println!("Dry run: nothing was deleted. Re-run with --delete to remove these directories.");
    }
}
