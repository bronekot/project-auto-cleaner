use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{Display, Formatter};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type ProjectId = usize;

const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectKind {
    Cargo,
    JavaScript,
}

impl Display for ProjectKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cargo => write!(f, "cargo"),
            Self::JavaScript => write!(f, "javascript"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: ProjectId,
    pub root: PathBuf,
    pub kinds: BTreeSet<ProjectKind>,
    pub markers: Vec<PathBuf>,
}

impl Project {
    fn has_kind(&self, kind: ProjectKind) -> bool {
        self.kinds.contains(&kind)
    }

    fn cargo_manifest(&self) -> Option<&Path> {
        self.markers.iter().find_map(|path| {
            (path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml"))
                .then_some(path.as_path())
        })
    }
}

#[derive(Debug, Clone)]
pub struct ActivityError {
    pub path: PathBuf,
    pub message: String,
}

impl Display for ActivityError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

#[derive(Debug, Clone)]
pub enum ProjectStatus {
    Fresh { latest_activity: SystemTime },
    Stale { latest_activity: SystemTime },
    Unverified { errors: Vec<ActivityError> },
}

impl ProjectStatus {
    pub fn is_stale(&self) -> bool {
        matches!(self, Self::Stale { .. })
    }

    pub fn latest_activity(&self) -> Option<SystemTime> {
        match self {
            Self::Fresh { latest_activity } | Self::Stale { latest_activity } => {
                Some(*latest_activity)
            }
            Self::Unverified { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CleanupKind {
    CargoTarget,
    NodeModules,
    YarnCache,
    YarnUnplugged,
}

impl Display for CleanupKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CargoTarget => write!(f, "target"),
            Self::NodeModules => write!(f, "node_modules"),
            Self::YarnCache => write!(f, ".yarn/cache"),
            Self::YarnUnplugged => write!(f, ".yarn/unplugged"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CleanupTarget {
    pub path: PathBuf,
    pub canonical_path: Option<PathBuf>,
    pub kind: CleanupKind,
    pub owners: Vec<ProjectId>,
    ownership_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunError {
    pub path: Option<PathBuf>,
    pub message: String,
}

impl Display for RunError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}", path.display(), self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

#[derive(Debug)]
pub enum CleanerError {
    InvalidRoot(String),
    Io { path: PathBuf, source: io::Error },
}

impl Display for CleanerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRoot(message) => write!(f, "invalid root: {message}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for CleanerError {}

#[derive(Debug)]
pub struct CleanupPlan {
    pub root: PathBuf,
    pub canonical_root: PathBuf,
    pub now: SystemTime,
    pub threshold: Duration,
    pub projects: Vec<Project>,
    pub statuses: BTreeMap<ProjectId, ProjectStatus>,
    pub targets: Vec<CleanupTarget>,
    pub errors: Vec<RunError>,
}

#[derive(Debug, Clone)]
pub struct ProjectReport {
    pub project: Project,
    pub status: ProjectStatus,
}

#[derive(Debug, Clone)]
pub enum TargetOutcome {
    WouldDelete,
    Deleted,
    Protected(String),
    Failed(String),
}

impl TargetOutcome {
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

#[derive(Debug, Clone)]
pub struct TargetReport {
    pub target: CleanupTarget,
    pub outcome: TargetOutcome,
}

#[derive(Debug)]
pub struct RunReport {
    pub projects: Vec<ProjectReport>,
    pub targets: Vec<TargetReport>,
    pub errors: Vec<RunError>,
}

impl RunReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
            || self
                .targets
                .iter()
                .any(|target| target.outcome.is_failure())
    }
}

#[derive(Debug, Clone)]
struct DependencyCandidate {
    path: PathBuf,
    kind: CleanupKind,
}

#[derive(Debug, Default)]
struct DiscoveryState {
    markers: BTreeMap<PathBuf, BTreeSet<ProjectKind>>,
    marker_paths: BTreeMap<PathBuf, Vec<PathBuf>>,
    dependency_candidates: BTreeSet<(PathBuf, CleanupKind)>,
    errors: Vec<RunError>,
}

#[derive(Debug, Clone)]
struct WorkspaceSpec {
    members: WorkspaceGlobs,
    excludes: WorkspaceGlobs,
}

#[derive(Debug, Clone)]
struct WorkspaceGlobs {
    patterns: Vec<Vec<String>>,
}

impl WorkspaceGlobs {
    fn is_match(&self, path: &str) -> bool {
        let path = path.split('/').collect::<Vec<_>>();
        self.patterns
            .iter()
            .any(|pattern| glob_segments_match(pattern, &path))
    }
}

#[derive(Debug)]
enum CargoOwnership {
    Single,
    Workspace(WorkspaceSpec),
    Unverified(String),
}

pub fn seconds_per_day() -> u64 {
    SECONDS_PER_DAY
}

pub fn determine_activity(
    project: &Project,
    all_projects: &[Project],
    now: SystemTime,
    threshold: Duration,
) -> ProjectStatus {
    if now.checked_sub(threshold).is_none() {
        return ProjectStatus::Unverified {
            errors: vec![ActivityError {
                path: project.root.clone(),
                message: "activity threshold is earlier than supported system time".into(),
            }],
        };
    }

    let nested_roots: Vec<&Path> = all_projects
        .iter()
        .filter(|candidate| {
            candidate.id != project.id && is_strict_descendant(&project.root, &candidate.root)
        })
        .map(|candidate| candidate.root.as_path())
        .collect();

    let mut latest_activity = None;
    let mut errors = Vec::new();
    walk_activity(
        &project.root,
        &project.root,
        &nested_roots,
        &mut latest_activity,
        &mut errors,
    );

    if !errors.is_empty() {
        return ProjectStatus::Unverified { errors };
    }

    let Some(latest_activity) = latest_activity else {
        return ProjectStatus::Unverified {
            errors: vec![ActivityError {
                path: project.root.clone(),
                message: "no readable regular files found".into(),
            }],
        };
    };

    let stale = activity_is_stale(latest_activity, now, threshold).unwrap_or(false);
    if stale {
        ProjectStatus::Stale { latest_activity }
    } else {
        ProjectStatus::Fresh { latest_activity }
    }
}

fn activity_is_stale(
    latest_activity: SystemTime,
    now: SystemTime,
    threshold: Duration,
) -> Option<bool> {
    now.checked_sub(threshold)
        .map(|cutoff| latest_activity < cutoff)
}

fn walk_activity(
    directory: &Path,
    project_root: &Path,
    nested_roots: &[&Path],
    latest_activity: &mut Option<SystemTime>,
    errors: &mut Vec<ActivityError>,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(activity_error(directory, error));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(activity_error(directory, error));
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                errors.push(activity_error(&path, error));
                continue;
            }
        };

        if is_link_or_reparse(&metadata) {
            continue;
        }

        if metadata.is_dir() {
            if path != project_root && nested_roots.iter().any(|nested_root| path == *nested_root) {
                continue;
            }

            if is_ignored_directory(&path) {
                continue;
            }

            walk_activity(&path, project_root, nested_roots, latest_activity, errors);
        } else if metadata.is_file() {
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    errors.push(activity_error(&path, error));
                    continue;
                }
            };
            if latest_activity.is_none_or(|latest| modified > latest) {
                *latest_activity = Some(modified);
            }
        }
    }
}

fn activity_error(path: &Path, error: io::Error) -> ActivityError {
    ActivityError {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

pub fn build_cleanup_plan(
    root: impl AsRef<Path>,
    now: SystemTime,
    threshold: Duration,
) -> Result<CleanupPlan, CleanerError> {
    let input_root = root.as_ref();
    let metadata = fs::symlink_metadata(input_root).map_err(|source| CleanerError::Io {
        path: input_root.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(CleanerError::InvalidRoot(format!(
            "{} is not a regular directory",
            input_root.display()
        )));
    }
    let canonical_root = fs::canonicalize(input_root).map_err(|source| CleanerError::Io {
        path: input_root.to_path_buf(),
        source,
    })?;

    let mut discovery = DiscoveryState::default();
    walk_discovery(&canonical_root, &mut discovery);

    let projects = make_projects(&discovery);
    let statuses = projects
        .iter()
        .map(|project| {
            (
                project.id,
                determine_activity(project, &projects, now, threshold),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut targets = Vec::new();
    let mut errors = discovery.errors;

    for project in &projects {
        if !project.has_kind(ProjectKind::Cargo) {
            continue;
        }
        let target_path = project.root.join("target");
        if !path_is_directory_or_reparse(&target_path, &mut errors) {
            continue;
        }

        let ownership = cargo_ownership(project);
        let (owners, ownership_error) = match ownership {
            CargoOwnership::Single => (vec![project.id], None),
            CargoOwnership::Workspace(spec) => (workspace_owners(project, &projects, &spec), None),
            CargoOwnership::Unverified(message) => (vec![project.id], Some(message)),
        };
        targets.push(make_target(
            &target_path,
            CleanupKind::CargoTarget,
            owners,
            ownership_error,
        ));
    }

    let dependency_candidates = discovery
        .dependency_candidates
        .into_iter()
        .map(|(path, kind)| DependencyCandidate { path, kind })
        .collect::<Vec<_>>();
    targets.extend(make_javascript_targets(
        &dependency_candidates,
        &projects,
        &mut errors,
    ));

    targets.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
    });

    Ok(CleanupPlan {
        root: input_root.to_path_buf(),
        canonical_root,
        now,
        threshold,
        projects,
        statuses,
        targets,
        errors,
    })
}

fn walk_discovery(directory: &Path, state: &mut DiscoveryState) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            state.errors.push(run_error(directory, error));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                state.errors.push(run_error(directory, error));
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                state.errors.push(run_error(&path, error));
                continue;
            }
        };

        if metadata.is_dir() {
            if let Some(kind) = dependency_kind(&path) {
                state.dependency_candidates.insert((path.clone(), kind));
                continue;
            }
            if is_ignored_directory(&path) || is_link_or_reparse(&metadata) {
                continue;
            }
            walk_discovery(&path, state);
            continue;
        }

        if !metadata.is_file() || is_link_or_reparse(&metadata) {
            continue;
        }

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let (kind, expected_name) = match name {
            "Cargo.toml" => (ProjectKind::Cargo, "Cargo.toml"),
            "package.json" => (ProjectKind::JavaScript, "package.json"),
            _ => continue,
        };
        debug_assert_eq!(name, expected_name);
        let Some(project_root) = path.parent() else {
            continue;
        };
        let project_root = project_root.to_path_buf();
        state
            .markers
            .entry(project_root.clone())
            .or_default()
            .insert(kind);
        state
            .marker_paths
            .entry(project_root)
            .or_default()
            .push(path);
    }
}

fn make_projects(discovery: &DiscoveryState) -> Vec<Project> {
    discovery
        .markers
        .iter()
        .enumerate()
        .map(|(id, (root, kinds))| Project {
            id,
            root: root.clone(),
            kinds: kinds.clone(),
            markers: discovery
                .marker_paths
                .get(root)
                .cloned()
                .unwrap_or_default(),
        })
        .collect()
}

fn cargo_ownership(project: &Project) -> CargoOwnership {
    let Some(manifest) = project.cargo_manifest() else {
        return CargoOwnership::Unverified("Cargo manifest was not discovered".into());
    };

    let content = match fs::read_to_string(manifest) {
        Ok(content) => content,
        Err(error) => {
            return CargoOwnership::Unverified(format!("cannot read workspace manifest: {error}"))
        }
    };
    match parse_workspace_spec(&content) {
        Ok(Some(spec)) => CargoOwnership::Workspace(spec),
        Ok(None) => CargoOwnership::Single,
        Err(message) => CargoOwnership::Unverified(message),
    }
}

fn parse_workspace_spec(content: &str) -> Result<Option<WorkspaceSpec>, String> {
    let mut section = None::<String>;
    let mut found_workspace = false;
    let mut members = None::<Vec<String>>;
    let mut excludes = None::<Vec<String>>;
    let mut pending = None::<(String, String)>;

    for raw_line in content.lines() {
        let line = strip_toml_comment(raw_line).trim().to_owned();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') {
            if !line.ends_with(']') {
                return Err("malformed Cargo.toml table header".into());
            }
            if pending.is_some() {
                return Err("workspace array is not closed".into());
            }
            let name = line[1..line.len() - 1].trim();
            section = Some(name.to_owned());
            if name == "workspace" {
                found_workspace = true;
            }
            continue;
        }

        if section.as_deref() != Some("workspace") {
            continue;
        }

        if let Some((field, value)) = &mut pending {
            value.push(' ');
            value.push_str(&line);
            if bracket_balance(value) < 0 {
                return Err(format!("workspace.{field} has an invalid array"));
            }
            if bracket_balance(value) == 0 {
                let parsed = parse_workspace_array(value, field)?;
                if field == "members" {
                    members = Some(parsed);
                } else {
                    excludes = Some(parsed);
                }
                pending = None;
            }
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err("invalid assignment in [workspace]".into());
        };
        let key = key.trim();
        if key != "members" && key != "exclude" {
            continue;
        }
        let value = value.trim().to_owned();
        if bracket_balance(&value) < 0 {
            return Err(format!("workspace.{key} has an invalid array"));
        }
        if bracket_balance(&value) > 0 {
            pending = Some((key.to_owned(), value));
            continue;
        }
        let parsed = parse_workspace_array(&value, key)?;
        if key == "members" {
            members = Some(parsed);
        } else {
            excludes = Some(parsed);
        }
    }

    if pending.is_some() {
        return Err("workspace array is not closed".into());
    }
    if !found_workspace {
        return Ok(None);
    }

    Ok(Some(WorkspaceSpec {
        members: workspace_globs(members.unwrap_or_default(), "members")?,
        excludes: workspace_globs(excludes.unwrap_or_default(), "exclude")?,
    }))
}

fn workspace_globs(values: Vec<String>, name: &str) -> Result<WorkspaceGlobs, String> {
    let mut patterns = Vec::with_capacity(values.len());
    for value in values {
        let pattern = value.replace('\\', "/");
        if Path::new(&pattern).is_absolute() {
            return Err(format!("workspace.{name} contains an absolute pattern"));
        }
        let segments = pattern
            .trim_start_matches("./")
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if segments.is_empty() {
            return Err(format!("workspace.{name} contains an empty pattern"));
        }
        patterns.push(segments);
    }
    Ok(WorkspaceGlobs { patterns })
}

fn parse_workspace_array(value: &str, name: &str) -> Result<Vec<String>, String> {
    let value = value.trim();
    if !value.starts_with('[') || !value.ends_with(']') {
        return Err(format!("workspace.{name} must be an array of strings"));
    }
    let inner = &value[1..value.len() - 1];
    let mut result = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(character) if character.is_whitespace() || *character == ',')
        {
            chars.next();
        }
        let Some(quote) = chars.next() else {
            break;
        };
        if quote != '"' && quote != '\'' {
            return Err(format!("workspace.{name} contains a non-string pattern"));
        }
        let mut pattern = String::new();
        let mut closed = false;
        while let Some(character) = chars.next() {
            if character == quote {
                closed = true;
                break;
            }
            if character == '\\' && quote == '"' {
                let Some(escaped) = chars.next() else {
                    return Err(format!("workspace.{name} contains an unfinished escape"));
                };
                pattern.push(escaped);
            } else {
                pattern.push(character);
            }
        }
        if !closed {
            return Err(format!("workspace.{name} contains an unterminated string"));
        }
        result.push(pattern);
        while matches!(chars.peek(), Some(character) if character.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            Some(',') => {
                chars.next();
            }
            None => break,
            Some(_) => return Err(format!("workspace.{name} has invalid array syntax")),
        }
    }
    Ok(result)
}

fn strip_toml_comment(line: &str) -> String {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote == Some('"') {
            escaped = true;
            continue;
        }
        if character == '"' || character == '\'' {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if character == '#' && quote.is_none() {
            return line[..index].to_owned();
        }
    }
    line.to_owned()
}

fn bracket_balance(value: &str) -> i32 {
    let mut balance = 0;
    let mut quote = None;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote == Some('"') {
            escaped = true;
            continue;
        }
        if character == '"' || character == '\'' {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_none() {
            match character {
                '[' => balance += 1,
                ']' => balance -= 1,
                _ => {}
            }
        }
    }
    balance
}

fn glob_segments_match(pattern: &[String], path: &[&str]) -> bool {
    fn visit(
        pattern: &[String],
        path: &[&str],
        pattern_index: usize,
        path_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, path_index)) {
            return *result;
        }
        let result = if pattern_index == pattern.len() {
            path_index == path.len()
        } else if pattern[pattern_index] == "**" {
            visit(pattern, path, pattern_index + 1, path_index, memo)
                || (path_index < path.len()
                    && visit(pattern, path, pattern_index, path_index + 1, memo))
        } else {
            path_index < path.len()
                && segment_matches(&pattern[pattern_index], path[path_index])
                && visit(pattern, path, pattern_index + 1, path_index + 1, memo)
        };
        memo.insert((pattern_index, path_index), result);
        result
    }

    visit(pattern, path, 0, 0, &mut HashMap::new())
}

fn segment_matches(pattern: &str, value: &str) -> bool {
    fn visit(
        pattern: &[char],
        value: &[char],
        pattern_index: usize,
        value_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, value_index)) {
            return *result;
        }
        let result = match pattern.get(pattern_index) {
            None => value_index == value.len(),
            Some('*') => {
                visit(pattern, value, pattern_index + 1, value_index, memo)
                    || (value_index < value.len()
                        && visit(pattern, value, pattern_index, value_index + 1, memo))
            }
            Some('?') => {
                value_index < value.len()
                    && visit(pattern, value, pattern_index + 1, value_index + 1, memo)
            }
            Some(expected) => {
                value.get(value_index) == Some(expected)
                    && visit(pattern, value, pattern_index + 1, value_index + 1, memo)
            }
        };
        memo.insert((pattern_index, value_index), result);
        result
    }

    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    visit(&pattern, &value, 0, 0, &mut HashMap::new())
}

fn workspace_owners(
    workspace_root: &Project,
    projects: &[Project],
    spec: &WorkspaceSpec,
) -> Vec<ProjectId> {
    let mut owners = vec![workspace_root.id];
    for project in projects {
        if project.id == workspace_root.id || !project.has_kind(ProjectKind::Cargo) {
            continue;
        }
        let Some(relative) = slash_relative_path(&workspace_root.root, &project.root) else {
            continue;
        };
        if spec.members.is_match(&relative) && !spec.excludes.is_match(&relative) {
            owners.push(project.id);
        }
    }
    owners.sort_unstable();
    owners.dedup();
    owners
}

fn make_javascript_targets(
    candidates: &[DependencyCandidate],
    projects: &[Project],
    errors: &mut Vec<RunError>,
) -> Vec<CleanupTarget> {
    let mut targets = Vec::new();
    for candidate in candidates {
        if !path_is_directory_or_reparse(&candidate.path, errors) {
            continue;
        }

        let Some(owner) = nearest_javascript_owner(&candidate.path, projects) else {
            continue;
        };
        let mut owners = vec![owner.id];
        for project in projects {
            if project.id == owner.id
                || !project.has_kind(ProjectKind::JavaScript)
                || !is_strict_descendant(&owner.root, &project.root)
            {
                continue;
            }
            if !has_corresponding_dependency_between(&project.root, &owner.root, candidate.kind) {
                owners.push(project.id);
            }
        }
        owners.sort_unstable();
        owners.dedup();
        targets.push(make_target(&candidate.path, candidate.kind, owners, None));
    }
    targets
}

fn nearest_javascript_owner<'a>(path: &Path, projects: &'a [Project]) -> Option<&'a Project> {
    projects
        .iter()
        .filter(|project| {
            project.has_kind(ProjectKind::JavaScript) && is_strict_descendant(&project.root, path)
        })
        .max_by_key(|project| project.root.components().count())
}

fn has_corresponding_dependency_between(
    nested_root: &Path,
    owner_root: &Path,
    kind: CleanupKind,
) -> bool {
    let mut current = nested_root.to_path_buf();
    loop {
        if current == owner_root {
            break;
        }
        let corresponding = dependency_path(&current, kind);
        if path_exists_without_following(&corresponding) {
            return true;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    false
}

fn make_target(
    path: &Path,
    kind: CleanupKind,
    owners: Vec<ProjectId>,
    ownership_error: Option<String>,
) -> CleanupTarget {
    CleanupTarget {
        path: path.to_path_buf(),
        canonical_path: fs::canonicalize(path).ok(),
        kind,
        owners,
        ownership_error,
    }
}

pub fn execute_plan(plan: &CleanupPlan, dry_run: bool) -> RunReport {
    let projects = plan
        .projects
        .iter()
        .filter_map(|project| {
            plan.statuses
                .get(&project.id)
                .cloned()
                .map(|status| ProjectReport {
                    project: project.clone(),
                    status,
                })
        })
        .collect();

    let mut errors = plan.errors.clone();
    let mut target_reports = Vec::new();

    for target in &plan.targets {
        if let Some(reason) = target_protection_reason(target, plan) {
            target_reports.push(TargetReport {
                target: target.clone(),
                outcome: TargetOutcome::Protected(reason),
            });
            continue;
        }

        let mut target = target.clone();
        match validate_target(&plan.canonical_root, &mut target) {
            Ok(()) if dry_run => {
                target_reports.push(TargetReport {
                    target,
                    outcome: TargetOutcome::WouldDelete,
                });
            }
            Ok(()) => match fs::remove_dir_all(&target.path) {
                Ok(()) => target_reports.push(TargetReport {
                    target,
                    outcome: TargetOutcome::Deleted,
                }),
                Err(error) => {
                    let message = error.to_string();
                    errors.push(RunError {
                        path: Some(target.path.clone()),
                        message: message.clone(),
                    });
                    target_reports.push(TargetReport {
                        target,
                        outcome: TargetOutcome::Failed(message),
                    });
                }
            },
            Err(message) => {
                errors.push(RunError {
                    path: Some(target.path.clone()),
                    message: message.clone(),
                });
                target_reports.push(TargetReport {
                    target,
                    outcome: TargetOutcome::Failed(message),
                });
            }
        }
    }

    RunReport {
        projects,
        targets: target_reports,
        errors,
    }
}

fn target_protection_reason(target: &CleanupTarget, plan: &CleanupPlan) -> Option<String> {
    if let Some(error) = &target.ownership_error {
        return Some(format!("ownership is unverified: {error}"));
    }
    if target.owners.is_empty() {
        return Some("target has no verified owners".into());
    }
    for owner in &target.owners {
        match plan.statuses.get(owner) {
            Some(ProjectStatus::Stale { .. }) => {}
            Some(ProjectStatus::Fresh { .. }) => {
                return Some(format!("protected by fresh project #{owner}"));
            }
            Some(ProjectStatus::Unverified { .. }) => {
                return Some(format!("protected by unverified project #{owner}"));
            }
            None => return Some(format!("owner project #{owner} was not found")),
        }
    }
    None
}

fn validate_target(canonical_root: &Path, target: &mut CleanupTarget) -> Result<(), String> {
    let metadata = fs::symlink_metadata(&target.path)
        .map_err(|error| format!("cannot inspect target: {error}"))?;
    if !metadata.is_dir() {
        return Err("target is not a directory".into());
    }
    if is_link_or_reparse(&metadata) {
        return Err("target is a symlink or reparse point".into());
    }
    let canonical_target = fs::canonicalize(&target.path)
        .map_err(|error| format!("cannot canonicalize target: {error}"))?;
    if !is_strict_descendant(canonical_root, &canonical_target) {
        return Err("target is not a strict descendant of ROOT".into());
    }
    target.canonical_path = Some(canonical_target);
    Ok(())
}

fn path_is_directory_or_reparse(path: &Path, errors: &mut Vec<RunError>) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_dir(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            errors.push(run_error(path, error));
            false
        }
    }
}

fn path_exists_without_following(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn dependency_path(root: &Path, kind: CleanupKind) -> PathBuf {
    match kind {
        CleanupKind::NodeModules => root.join("node_modules"),
        CleanupKind::YarnCache => root.join(".yarn").join("cache"),
        CleanupKind::YarnUnplugged => root.join(".yarn").join("unplugged"),
        CleanupKind::CargoTarget => root.join("target"),
    }
}

fn dependency_kind(path: &Path) -> Option<CleanupKind> {
    let name = path.file_name().and_then(|name| name.to_str())?;
    if name == "node_modules" {
        return Some(CleanupKind::NodeModules);
    }
    if name == "cache"
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some(".yarn")
    {
        return Some(CleanupKind::YarnCache);
    }
    if name == "unplugged"
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some(".yarn")
    {
        return Some(CleanupKind::YarnUnplugged);
    }
    None
}

fn is_ignored_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git"
                | ".hg"
                | ".svn"
                | "target"
                | "node_modules"
                | "dist"
                | "build"
                | "coverage"
                | ".cache"
                | ".next"
                | ".nuxt"
                | ".turbo"
                | ".parcel-cache"
        )
    )
}

fn is_strict_descendant(root: &Path, candidate: &Path) -> bool {
    candidate != root && candidate.starts_with(root)
}

fn slash_relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut result = String::new();
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return None;
        }
        if !result.is_empty() {
            result.push('/');
        }
        result.push_str(component.as_os_str().to_string_lossy().as_ref());
    }
    Some(result)
}

fn run_error(path: &Path, error: io::Error) -> RunError {
    RunError {
        path: Some(path.to_path_buf()),
        message: error.to_string(),
    }
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink() || is_windows_reparse_point(metadata)
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

pub fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}s since UNIX_EPOCH", duration.as_secs()),
        Err(error) => format!("{}s before UNIX_EPOCH", error.duration().as_secs()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    pub root: PathBuf,
    pub days: u64,
    pub dry_run: bool,
    pub verbose: bool,
    pub help: bool,
    pub version: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError(pub String);

impl Display for CliError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CliError {}

pub fn parse_cli<I, S>(args: I) -> Result<CliOptions, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut arguments = args.into_iter().map(Into::into);
    let _program = arguments.next();
    let mut root = None;
    let mut days = 90;
    let mut dry_run = false;
    let mut verbose = false;
    let mut help = false;
    let mut version = false;
    let mut positional_only = false;

    while let Some(argument) = arguments.next() {
        if positional_only {
            set_root(&mut root, argument)?;
            continue;
        }
        match argument.as_str() {
            "--" => positional_only = true,
            "--dry-run" => dry_run = true,
            "--verbose" => verbose = true,
            "-h" | "--help" => help = true,
            "--version" => version = true,
            "--days" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| CliError("--days requires a positive integer".into()))?;
                days = parse_days(&value)?;
            }
            value if value.starts_with("--days=") => {
                days = parse_days(&value["--days=".len()..])?;
            }
            value if value.starts_with('-') => {
                return Err(CliError(format!("unknown option: {value}")));
            }
            value => set_root(&mut root, value.to_owned())?,
        }
    }

    if help || version {
        return Ok(CliOptions {
            root: root.unwrap_or_default(),
            days,
            dry_run,
            verbose,
            help,
            version,
        });
    }

    let root = root.ok_or_else(|| CliError("ROOT is required".into()))?;
    Ok(CliOptions {
        root,
        days,
        dry_run,
        verbose,
        help,
        version,
    })
}

fn set_root(root: &mut Option<PathBuf>, value: String) -> Result<(), CliError> {
    if root.is_some() {
        return Err(CliError("only one ROOT is allowed".into()));
    }
    *root = Some(PathBuf::from(value));
    Ok(())
}

fn parse_days(value: &str) -> Result<u64, CliError> {
    let days = value
        .parse::<u64>()
        .map_err(|_| CliError("--days requires a positive integer".into()))?;
    if days == 0 {
        return Err(CliError("--days requires a positive integer".into()));
    }
    Ok(days)
}

pub fn threshold_for_days(days: u64) -> Result<Duration, CliError> {
    days.checked_mul(SECONDS_PER_DAY)
        .map(Duration::from_secs)
        .ok_or_else(|| CliError("--days value is too large".into()))
}

pub fn usage() -> &'static str {
    "Usage: project-auto-cleaner <ROOT> [--days N] [--dry-run] [--verbose]\n\n\
Default: clean projects whose latest relevant file is older than 90 days.\n\
Use --dry-run to inspect the plan without deleting anything."
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "project-auto-cleaner-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn exact_threshold_is_fresh() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let threshold = Duration::from_secs(90);
        let latest = now - threshold;
        assert_eq!(activity_is_stale(latest, now, threshold), Some(false));
    }

    fn set_modified(path: &Path, modified: SystemTime) {
        let file = File::options().write(true).open(path).unwrap();
        let times = fs::FileTimes::new().set_modified(modified);
        file.set_times(times).unwrap();
    }

    #[test]
    fn nested_project_does_not_make_parent_fresh() {
        let temp = TestDir::new();
        let root = temp.path.as_path();
        write_file(
            &root.join("Cargo.toml"),
            "[package]\nname='old'\nversion='0.1.0'\n",
        );
        write_file(&root.join("src/lib.rs"), "old");
        write_file(
            &root.join("experiments/package.json"),
            "{\"name\":\"experiment\"}",
        );
        write_file(&root.join("experiments/src/index.js"), "new");
        let old = SystemTime::now() - Duration::from_secs(180 * SECONDS_PER_DAY);
        set_modified(&root.join("Cargo.toml"), old);
        set_modified(&root.join("src/lib.rs"), old);

        let mut discovery = DiscoveryState::default();
        walk_discovery(root, &mut discovery);
        let projects = make_projects(&discovery);
        assert_eq!(projects.len(), 2);

        let parent = projects
            .iter()
            .find(|project| project.root == root)
            .unwrap();
        let status = determine_activity(
            parent,
            &projects,
            SystemTime::now(),
            Duration::from_secs(90 * SECONDS_PER_DAY),
        );
        assert!(matches!(status, ProjectStatus::Stale { .. }));
    }

    #[test]
    fn cli_parses_options() {
        let options = parse_cli([
            "project-auto-cleaner",
            "projects",
            "--days=30",
            "--dry-run",
            "--verbose",
        ])
        .unwrap();
        assert_eq!(options.root, PathBuf::from("projects"));
        assert_eq!(options.days, 30);
        assert!(options.dry_run);
        assert!(options.verbose);
    }

    #[test]
    fn workspace_members_are_resolved_without_unrelated_projects() {
        let temp = TestDir::new();
        let root = temp.path.as_path();
        write_file(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers=['crates/*']\nexclude=['crates/excluded']\n",
        );
        write_file(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname='foo'\n",
        );
        write_file(
            &root.join("crates/excluded/Cargo.toml"),
            "[package]\nname='excluded'\n",
        );
        write_file(
            &root.join("experiments/test/Cargo.toml"),
            "[package]\nname='test'\n",
        );

        let mut discovery = DiscoveryState::default();
        walk_discovery(root, &mut discovery);
        let projects = make_projects(&discovery);
        let workspace = projects
            .iter()
            .find(|project| project.root == root)
            .unwrap();
        let ownership = cargo_ownership(workspace);
        let CargoOwnership::Workspace(spec) = ownership else {
            panic!("expected workspace");
        };
        let owners = workspace_owners(workspace, &projects, &spec);
        let owner_roots = owners
            .iter()
            .map(|id| projects[*id].root.clone())
            .collect::<Vec<_>>();
        assert!(owner_roots.contains(&root.to_path_buf()));
        assert!(owner_roots.contains(&root.join("crates/foo")));
        assert!(!owner_roots.contains(&root.join("crates/excluded")));
        assert!(!owner_roots.contains(&root.join("experiments/test")));
    }

    #[test]
    fn nested_js_project_owns_parent_dependency_only_without_own_dependency_dir() {
        let temp = TestDir::new();
        let root = temp.path.as_path();
        let nested = root.join("packages/app");
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        assert!(!has_corresponding_dependency_between(
            &nested,
            root,
            CleanupKind::NodeModules
        ));
        fs::create_dir_all(nested.join("node_modules")).unwrap();
        assert!(has_corresponding_dependency_between(
            &nested,
            root,
            CleanupKind::NodeModules
        ));
    }

    #[test]
    fn stale_cargo_target_is_deleted_but_manifest_is_preserved() {
        let temp = TestDir::new();
        let root = temp.path.as_path();
        let manifest = root.join("Cargo.toml");
        let target = root.join("target");
        write_file(&manifest, "[package]\nname='old'\nversion='0.1.0'\n");
        write_file(&target.join("debug/artifact"), "artifact");
        set_modified(
            &manifest,
            SystemTime::now() - Duration::from_secs(180 * SECONDS_PER_DAY),
        );

        let plan = build_cleanup_plan(
            root,
            SystemTime::now(),
            Duration::from_secs(90 * SECONDS_PER_DAY),
        )
        .unwrap();
        let report = execute_plan(&plan, false);

        assert!(!target.exists());
        assert!(manifest.exists());
        assert!(!report.has_errors());
        assert!(matches!(report.targets[0].outcome, TargetOutcome::Deleted));
    }

    #[test]
    fn target_must_be_strict_descendant_of_root() {
        let temp = TestDir::new();
        let root = fs::canonicalize(temp.path.as_path()).unwrap();
        let mut target = CleanupTarget {
            path: root.clone(),
            canonical_path: None,
            kind: CleanupKind::CargoTarget,
            owners: vec![],
            ownership_error: None,
        };
        assert!(validate_target(&root, &mut target).is_err());
    }
}
