mod report;

use project_auto_cleaner::{
    build_cleanup_plan, execute_plan, parse_cli, threshold_for_days, usage, ScanOptions,
};
use std::io;
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
    let dry_run = !options.delete;
    let report = execute_plan(&plan, dry_run);
    if let Err(error) = report::write_report(
        &mut io::stdout().lock(),
        &plan,
        &report,
        dry_run,
        options.verbose,
    ) {
        // Piping a report through head should not produce a panic.
        if error.kind() != io::ErrorKind::BrokenPipe {
            return Err(format!("cannot write report: {error}"));
        }
    }

    if report.has_errors() {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
