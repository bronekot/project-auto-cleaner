use project_auto_cleaner::{
    build_cleanup_plan, execute_plan, format_system_time, parse_cli, threshold_for_days, usage,
    ProjectStatus, TargetOutcome,
};
use std::process::ExitCode;
use std::time::SystemTime;

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
    let options = parse_cli(std::env::args()).map_err(|error| error.to_string())?;
    if options.help {
        println!("{}", usage());
        return Ok(ExitCode::SUCCESS);
    }
    if options.version {
        println!("project-auto-cleaner {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    let threshold = threshold_for_days(options.days).map_err(|error| error.to_string())?;
    let now = SystemTime::now();
    let plan =
        build_cleanup_plan(&options.root, now, threshold).map_err(|error| error.to_string())?;
    let report = execute_plan(&plan, options.dry_run);
    print_report(&report, options.dry_run, options.verbose);

    if report.has_errors() {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn print_report(report: &project_auto_cleaner::RunReport, dry_run: bool, verbose: bool) {
    println!("Projects: {}", report.projects.len());
    for project in &report.projects {
        match &project.status {
            ProjectStatus::Fresh { latest_activity } => {
                if verbose {
                    println!(
                        "  FRESH       {} ({})",
                        project.project.root.display(),
                        format_system_time(*latest_activity)
                    );
                }
            }
            ProjectStatus::Stale { latest_activity } => println!(
                "  STALE       {} ({})",
                project.project.root.display(),
                format_system_time(*latest_activity)
            ),
            ProjectStatus::Unverified { errors } => {
                println!(
                    "  UNVERIFIED  {} ({} error(s))",
                    project.project.root.display(),
                    errors.len()
                );
                for error in errors {
                    println!("               {error}");
                }
            }
        }
    }

    let action = if dry_run { "Would clean" } else { "Cleaned" };
    println!("Targets: {}", report.targets.len());
    for target in &report.targets {
        let label = match &target.outcome {
            TargetOutcome::WouldDelete => action,
            TargetOutcome::Deleted => "Deleted",
            TargetOutcome::Protected(_) => "Protected",
            TargetOutcome::Failed(_) => "Failed",
        };
        println!(
            "  {label:10} {:16} {}",
            target.target.kind,
            target.target.path.display()
        );
        match &target.outcome {
            TargetOutcome::Protected(reason) | TargetOutcome::Failed(reason) => {
                println!("               {reason}");
            }
            TargetOutcome::WouldDelete | TargetOutcome::Deleted => {}
        }
        if verbose {
            println!("               owners: {:?}", target.target.owners);
            if let Some(canonical_path) = &target.target.canonical_path {
                println!("               canonical: {}", canonical_path.display());
            }
        }
    }

    if !report.errors.is_empty() {
        println!("Errors: {}", report.errors.len());
        for error in &report.errors {
            println!("  {error}");
        }
    }
}
