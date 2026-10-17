use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

mod size;

pub type ProjectId = usize;

pub const DEFAULT_DAYS: u64 = 90;
const SECONDS_PER_DAY: u64 = 86_400;
/// Cargo writes this cache directory tag into every target directory it creates.
const CARGO_CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";
/// Never source code and never a place for projects, at any depth.
const VCS_DIRECTORIES: &[&str] = &[".git", ".hg", ".svn"];
/// Windows-reserved folders, skipped at any depth.
const SYSTEM_DIRECTORIES: &[&str] = &["$RECYCLE.BIN", "System Volume Information"];
/// Windows folders skipped only directly at the root of a volume.
const VOLUME_ROOT_SYSTEM_DIRECTORIES: &[&str] = &[
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "Recovery",
    "Windows",
];
/// Generated output. Ignored only directly inside a project root, so source
/// folders with the same name deeper in the tree still count as activity.
const GENERATED_DIRECTORIES: &[&str] = &[
    "dist",
    "build",
    "coverage",
    ".cache",
    ".next",
    ".nuxt",
    ".output",
    ".turbo",
    ".parcel-cache",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectKind {
    Cargo,
    JavaScript,
}

impl Display for ProjectKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            Self::Cargo => "cargo",
            Self::JavaScript => "javascript",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: ProjectId,
    pub root: PathBuf,
    pub markers: BTreeMap<ProjectKind, PathBuf>,
}

impl Project {
    /// Keep nested projects distinguishable without repeating the scan root.
    pub fn display_name(&self, root: &Path) -> String {
        slash_relative_path(root, &self.root)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                self.root.file_name().map_or_else(
                    || strip_verbatim(&self.root).display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
            })
    }

    pub fn has_kind(&self, kind: ProjectKind) -> bool {
        self.markers.contains_key(&kind)
    }

    fn cargo_manifest(&self) -> Option<&Path> {
        self.markers.get(&ProjectKind::Cargo).map(PathBuf::as_path)
    }
}

#[derive(Debug, Clone)]
pub struct ActivityError {
    pub path: PathBuf,
    pub message: String,
}

impl Display for ActivityError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}",
            strip_verbatim(&self.path).display(),
            self.message
        )
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
        f.pad(match self {
            Self::CargoTarget => "target",
            Self::NodeModules => "node_modules",
            Self::YarnCache => ".yarn/cache",
            Self::YarnUnplugged => ".yarn/unplugged",
        })
    }
}

#[derive(Debug, Clone)]
pub struct CleanupTarget {
    pub path: PathBuf,
    pub canonical_path: Option<PathBuf>,
    pub kind: CleanupKind,
    pub owners: Vec<ProjectId>,
    pub is_link: bool,
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
            Some(path) => write!(f, "{}: {}", strip_verbatim(path).display(), self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

#[derive(Debug)]
pub enum CleanerError {
    InvalidRoot(String),
    InvalidExclude(String),
    Io { path: PathBuf, source: io::Error },
}

impl Display for CleanerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRoot(message) => write!(f, "invalid root: {message}"),
            Self::InvalidExclude(message) => write!(f, "invalid exclude: {message}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for CleanerError {}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub now: SystemTime,
    pub threshold: Duration,
    /// Directories inside ROOT that are not scanned at all. Relative paths are
    /// resolved against ROOT.
    pub excludes: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct CleanupPlan {
    pub root: PathBuf,
    pub canonical_root: PathBuf,
    /// Options with canonical exclude paths.
    pub options: ScanOptions,
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
    /// Kept on purpose: an owner is active, the directory is a link, etc.
    Protected(String),
    /// Kept because safety could not be verified; reported as an error.
    Unverified(String),
    Failed(String),
}

impl TargetOutcome {
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Unverified(_) | Self::Failed(_))
    }
}

#[derive(Debug, Clone)]
pub struct TargetReport {
    pub target: CleanupTarget,
    pub outcome: TargetOutcome,
    /// Estimated reclaimable bytes before removal; unknown for kept targets.
    pub estimated_bytes: Option<u64>,
}

impl TargetReport {
    fn new(target: CleanupTarget, outcome: TargetOutcome) -> Self {
        Self {
            target,
            outcome,
            estimated_bytes: None,
        }
    }
}

#[derive(Debug)]
pub struct RunReport {
    pub root: PathBuf,
    pub projects: Vec<ProjectReport>,
    pub targets: Vec<TargetReport>,
    pub errors: Vec<RunError>,
}

impl RunReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
            || self
                .projects
                .iter()
                .any(|project| matches!(project.status, ProjectStatus::Unverified { .. }))
            || self.targets.iter().any(|target| target.outcome.is_error())
    }
}

#[derive(Debug, Default)]
struct DiscoveryState {
    projects: BTreeMap<PathBuf, BTreeMap<ProjectKind, PathBuf>>,
    dependency_candidates: BTreeSet<(PathBuf, CleanupKind)>,
    errors: Vec<RunError>,
}

#[derive(Debug, Clone)]
struct WorkspaceSpec {
    members: Vec<glob::Pattern>,
    /// Normalized `/`-separated paths relative to the workspace root.
    excludes: Vec<String>,
}

impl WorkspaceSpec {
    /// Mirrors Cargo: `exclude` entries are path prefixes (not globs), and a
    /// path that is also covered by an explicit `members` entry stays a member.
    fn is_excluded(&self, relative: &str) -> bool {
        let relative = Path::new(relative);
        self.excludes
            .iter()
            .any(|exclude| relative.starts_with(exclude))
            && !self
                .members
                .iter()
                .any(|member| relative.starts_with(member.as_str()))
    }
}

#[derive(Debug)]
enum CargoOwnership {
    Single,
    Workspace(WorkspaceSpec),
    Unverified(String),
}

struct DirectoryListing {
    entries: Vec<(PathBuf, fs::Metadata)>,
    errors: Vec<(PathBuf, io::Error)>,
}

fn list_directory(directory: &Path) -> DirectoryListing {
    let mut listing = DirectoryListing {
        entries: Vec::new(),
        errors: Vec::new(),
    };
    let read_dir = match fs::read_dir(directory) {
        Ok(read_dir) => read_dir,
        Err(error) => {
            listing.errors.push((directory.to_path_buf(), error));
            return listing;
        }
    };
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                listing.errors.push((directory.to_path_buf(), error));
                continue;
            }
        };
        let path = entry.path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) => listing.entries.push((path, metadata)),
            Err(error) => listing.errors.push((path, error)),
        }
    }
    listing
}

fn project_markers(entries: &[(PathBuf, fs::Metadata)]) -> BTreeMap<ProjectKind, PathBuf> {
    entries
        .iter()
        .filter(|(_, metadata)| metadata.is_file())
        .filter_map(|(path, _)| {
            let kind = match path.file_name()?.to_str()? {
                "Cargo.toml" => ProjectKind::Cargo,
                "package.json" => ProjectKind::JavaScript,
                _ => return None,
            };
            Some((kind, path.clone()))
        })
        .collect()
}

fn determine_activity(
    project: &Project,
    all_projects: &[Project],
    options: &ScanOptions,
) -> ProjectStatus {
    let Some(cutoff) = options.now.checked_sub(options.threshold) else {
        return ProjectStatus::Unverified {
            errors: vec![ActivityError {
                path: project.root.clone(),
                message: "activity threshold is earlier than supported system time".into(),
            }],
        };
    };

    let nested_roots: Vec<&Path> = all_projects
        .iter()
        .filter(|candidate| is_strict_descendant(&project.root, &candidate.root))
        .map(|candidate| candidate.root.as_path())
        .collect();

    let mut latest_activity = None;
    let mut errors = Vec::new();
    walk_activity(
        &project.root,
        &project.root,
        &nested_roots,
        &options.excludes,
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

    if is_before_cutoff(latest_activity, cutoff) {
        ProjectStatus::Stale { latest_activity }
    } else {
        ProjectStatus::Fresh { latest_activity }
    }
}

fn is_before_cutoff(latest_activity: SystemTime, cutoff: SystemTime) -> bool {
    latest_activity < cutoff
}

fn walk_activity(
    directory: &Path,
    project_root: &Path,
    nested_roots: &[&Path],
    excludes: &[PathBuf],
    latest_activity: &mut Option<SystemTime>,
    errors: &mut Vec<ActivityError>,
) {
    let listing = list_directory(directory);
    errors.extend(
        listing
            .errors
            .into_iter()
            .map(|(path, error)| activity_error(&path, error)),
    );
    let markers = project_markers(&listing.entries);

    for (path, metadata) in &listing.entries {
        if is_excluded(path, excludes) || nested_roots.contains(&path.as_path()) {
            continue;
        }
        let is_link = metadata.file_type().is_symlink();
        if (metadata.is_dir() || is_link) && is_skipped_directory(path, &markers) {
            continue;
        }
        if is_link {
            if links_within_project(path, project_root) {
                continue;
            }
            errors.push(ActivityError {
                path: path.clone(),
                message: "activity behind a symlink or junction cannot be verified".into(),
            });
            continue;
        }

        if metadata.is_dir() {
            walk_activity(
                path,
                project_root,
                nested_roots,
                excludes,
                latest_activity,
                errors,
            );
        } else if metadata.is_file() {
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    errors.push(activity_error(path, error));
                    continue;
                }
            };
            if latest_activity.is_none_or(|latest| modified > latest) {
                *latest_activity = Some(modified);
            }
        }
    }
}

/// A link that resolves inside the same project adds no activity of its own:
/// its target is walked directly, skipped, or verified as a nested project.
/// Package managers such as bun create such links inside their caches.
fn links_within_project(link: &Path, project_root: &Path) -> bool {
    fs::canonicalize(link).is_ok_and(|target| target.starts_with(project_root))
}

fn activity_error(path: &Path, error: io::Error) -> ActivityError {
    ActivityError {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

pub fn build_cleanup_plan(
    root: impl AsRef<Path>,
    options: &ScanOptions,
) -> Result<CleanupPlan, CleanerError> {
    let input_root = root.as_ref();
    let metadata = fs::symlink_metadata(input_root).map_err(|source| CleanerError::Io {
        path: input_root.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(CleanerError::InvalidRoot(format!(
            "{} is not a directory (links are not followed)",
            input_root.display()
        )));
    }
    let canonical_root = fs::canonicalize(input_root).map_err(|source| CleanerError::Io {
        path: input_root.to_path_buf(),
        source,
    })?;
    let options = ScanOptions {
        excludes: resolve_excludes(input_root, &canonical_root, &options.excludes)?,
        ..options.clone()
    };

    let mut discovery = DiscoveryState::default();
    walk_discovery(&canonical_root, &options.excludes, &mut discovery);

    let projects = make_projects(&discovery);
    let statuses = projects
        .iter()
        .map(|project| (project.id, determine_activity(project, &projects, &options)))
        .collect::<BTreeMap<_, _>>();

    let discovery_errors = discovery.errors.clone();
    let mut errors = discovery.errors;
    let mut targets = Vec::new();

    for project in projects
        .iter()
        .filter(|project| project.has_kind(ProjectKind::Cargo))
    {
        let target_path = project.root.join("target");
        let Some(is_link) = existing_directory(&target_path, &mut errors) else {
            continue;
        };
        let (mut owners, ownership_error) = match cargo_ownership(project) {
            CargoOwnership::Single => (vec![project.id], None),
            CargoOwnership::Workspace(spec) => match workspace_owners(project, &projects, &spec) {
                Ok(owners) => (owners, None),
                Err(message) => (vec![project.id], Some(message)),
            },
            CargoOwnership::Unverified(message) => (vec![project.id], Some(message)),
        };
        // `tauri dev` and `tauri build` rebuild the Rust side whenever the
        // frontend changes, so the enclosing JavaScript project owns it too.
        if is_tauri_project(project) {
            if let Some(frontend) = nearest_javascript_owner(&project.root, &projects) {
                owners.push(frontend.id);
                owners.sort_unstable();
                owners.dedup();
            }
        }
        targets.push(make_target(
            &target_path,
            CleanupKind::CargoTarget,
            owners,
            is_link,
            ownership_error,
        ));
    }

    for (path, kind) in &discovery.dependency_candidates {
        let Some(is_link) = existing_directory(path, &mut errors) else {
            continue;
        };
        let Some(owner) = nearest_javascript_owner(path, &projects) else {
            continue;
        };
        // Node resolves missing packages through every ancestor node_modules,
        // and nested projects of any kind (e.g. src-tauri) build through it.
        let owners = std::iter::once(owner.id)
            .chain(
                projects
                    .iter()
                    .filter(|project| is_strict_descendant(&owner.root, &project.root))
                    .map(|project| project.id),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        targets.push(make_target(path, *kind, owners, is_link, None));
    }

    for target in &mut targets {
        if target.ownership_error.is_none() {
            let owner_roots = target
                .owners
                .iter()
                .filter_map(|id| projects.get(*id))
                .map(|project| project.root.as_path())
                .collect::<Vec<_>>();
            target.ownership_error =
                incomplete_discovery_reason(&target.path, &owner_roots, &discovery_errors);
        }
    }
    targets.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
    });

    Ok(CleanupPlan {
        root: input_root.to_path_buf(),
        canonical_root,
        options,
        projects,
        statuses,
        targets,
        errors,
    })
}

fn resolve_excludes(
    input_root: &Path,
    canonical_root: &Path,
    excludes: &[PathBuf],
) -> Result<Vec<PathBuf>, CleanerError> {
    excludes
        .iter()
        .map(|exclude| {
            let path = if exclude.is_absolute() {
                exclude.clone()
            } else {
                input_root.join(exclude)
            };
            let canonical = canonical_entry(&path).map_err(|source| CleanerError::Io {
                path: path.clone(),
                source,
            })?;
            if !is_strict_descendant(canonical_root, &canonical) {
                return Err(CleanerError::InvalidExclude(format!(
                    "{} is not inside ROOT",
                    exclude.display()
                )));
            }
            Ok(canonical)
        })
        .collect()
}

/// Canonicalizes only the parent, so that a directory that cannot be opened
/// (such as `System Volume Information`) can still be excluded.
fn canonical_entry(path: &Path) -> io::Result<PathBuf> {
    fs::symlink_metadata(path)?;
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            Ok(fs::canonicalize(parent)?.join(name))
        }
        _ => fs::canonicalize(path),
    }
}

fn is_excluded(path: &Path, excludes: &[PathBuf]) -> bool {
    excludes.iter().any(|exclude| {
        if cfg!(windows) {
            exclude
                .to_string_lossy()
                .eq_ignore_ascii_case(&path.to_string_lossy())
        } else {
            exclude == path
        }
    })
}

/// An unreadable path can hide projects that own a target. Only targets whose
/// owners could be affected are blocked, so an unrelated unreadable folder
/// does not stop the whole cleanup.
fn incomplete_discovery_reason(
    target: &Path,
    owner_roots: &[&Path],
    discovery_errors: &[RunError],
) -> Option<String> {
    discovery_errors.iter().find_map(|error| {
        let Some(path) = &error.path else {
            return Some("project discovery is incomplete".to_owned());
        };
        let affects_target =
            target.starts_with(path) || owner_roots.iter().any(|root| path.starts_with(root));
        affects_target.then(|| {
            format!(
                "project discovery is incomplete at {}",
                strip_verbatim(path).display()
            )
        })
    })
}

fn walk_discovery(directory: &Path, excludes: &[PathBuf], state: &mut DiscoveryState) {
    let listing = list_directory(directory);
    state.errors.extend(
        listing
            .errors
            .into_iter()
            .map(|(path, error)| run_error(&path, error)),
    );
    let markers = project_markers(&listing.entries);

    for (path, metadata) in &listing.entries {
        if is_excluded(path, excludes) {
            continue;
        }
        if let Some(kind) = dependency_kind(path) {
            if metadata.is_dir() || metadata.file_type().is_symlink() {
                state.dependency_candidates.insert((path.clone(), kind));
            }
            continue;
        }
        if metadata.is_dir() && !is_skipped_directory(path, &markers) {
            walk_discovery(path, excludes, state);
        }
    }

    if !markers.is_empty() {
        state.projects.insert(directory.to_path_buf(), markers);
    }
}

fn make_projects(discovery: &DiscoveryState) -> Vec<Project> {
    discovery
        .projects
        .iter()
        .enumerate()
        .map(|(id, (root, markers))| Project {
            id,
            root: root.clone(),
            markers: markers.clone(),
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

fn read_manifest_text(project: &Project) -> Result<String, String> {
    let manifest = project
        .cargo_manifest()
        .ok_or("Cargo manifest was not discovered")?;
    fs::read_to_string(manifest)
        .map_err(|error| format!("cannot read {}: {error}", manifest.display()))
}

fn read_manifest(project: &Project) -> Result<toml::Table, String> {
    read_manifest_text(project)?
        .parse::<toml::Table>()
        .map_err(|error| format!("invalid manifest in {}: {error}", project.root.display()))
}

fn parse_workspace_spec(content: &str) -> Result<Option<WorkspaceSpec>, String> {
    let document = content
        .parse::<toml::Table>()
        .map_err(|error| format!("invalid Cargo.toml: {error}"))?;
    let Some(workspace) = document.get("workspace") else {
        return Ok(None);
    };
    let workspace = workspace.as_table().ok_or("workspace must be a table")?;

    let mut members = Vec::new();
    for value in workspace_strings(workspace, "members")? {
        // Members outside the workspace cannot be verified by a subtree scan.
        let path = normalize_workspace_path(value)
            .filter(|_| !value.is_empty())
            .ok_or_else(|| format!("workspace.members contains an unsupported path: {value}"))?;
        members.push(
            glob::Pattern::new(&path)
                .map_err(|error| format!("invalid workspace.members glob: {error}"))?,
        );
    }
    // Excludes that leave the workspace can never exclude a member.
    let excludes = workspace_strings(workspace, "exclude")?
        .into_iter()
        .filter_map(normalize_workspace_path)
        .collect();
    Ok(Some(WorkspaceSpec { members, excludes }))
}

fn workspace_strings<'a>(table: &'a toml::Table, name: &str) -> Result<Vec<&'a str>, String> {
    let Some(values) = table.get(name) else {
        return Ok(Vec::new());
    };
    values
        .as_array()
        .ok_or_else(|| format!("workspace.{name} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| format!("workspace.{name} must contain strings"))
        })
        .collect()
}

/// Returns a `/`-separated path relative to the workspace root, or `None` for
/// a path that may leave it (absolute, rooted, drive-relative or with `..`).
fn normalize_workspace_path(value: &str) -> Option<String> {
    let value = value.replace('\\', "/");
    if Path::new(&value).is_absolute()
        || value.starts_with('/')
        || value.contains(':')
        || value.split('/').any(|segment| segment == "..")
    {
        return None;
    }
    Some(
        value
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn glob_options() -> glob::MatchOptions {
    glob::MatchOptions {
        case_sensitive: !cfg!(windows),
        require_literal_separator: true,
        require_literal_leading_dot: false,
    }
}

fn workspace_owners(
    workspace_root: &Project,
    projects: &[Project],
    spec: &WorkspaceSpec,
) -> Result<Vec<ProjectId>, String> {
    let mut owners = BTreeSet::from([workspace_root.id]);
    let glob_root = strip_verbatim(&workspace_root.root);
    let glob_root = glob::Pattern::escape(glob_root.to_str().ok_or("workspace path is not UTF-8")?);

    // Expand members as well as matching discovered projects: unreadable, linked,
    // missing or ignored members must not silently disappear from ownership.
    for pattern in &spec.members {
        let mut matched = false;
        for entry in glob::glob_with(&format!("{glob_root}/{}", pattern.as_str()), glob_options())
            .map_err(|error| error.to_string())?
        {
            let entry = entry.map_err(|error| error.to_string())?;
            matched = true;
            let metadata = fs::symlink_metadata(&entry).map_err(|error| error.to_string())?;
            // Like Cargo, ignore plain files matched by a glob (README, .DS_Store).
            if metadata.is_file() {
                continue;
            }
            validate_directory_chain(&entry)?;
            let entry = fs::canonicalize(&entry).map_err(|error| error.to_string())?;
            let relative = slash_relative_path(&workspace_root.root, &entry)
                .ok_or("workspace member is outside workspace")?;
            if spec.is_excluded(&relative) {
                continue;
            }
            let member = find_cargo_project(projects, &entry).ok_or_else(|| {
                format!(
                    "workspace member was not verified: {}",
                    strip_verbatim(&entry).display()
                )
            })?;
            owners.insert(member.id);
        }
        if !matched {
            return Err(format!(
                "workspace member pattern has no verified matches: {}",
                pattern.as_str()
            ));
        }
    }

    // Explicit `package.workspace` pointers. Since members with `..` are
    // rejected, only packages inside the workspace root can be members.
    for project in projects.iter().filter(|project| {
        project.has_kind(ProjectKind::Cargo)
            && is_strict_descendant(&workspace_root.root, &project.root)
    }) {
        // Cargo cannot load an unparseable manifest as a member either, but an
        // unreadable one may be valid and must block deletion.
        let Ok(document) = read_manifest_text(project)?.parse::<toml::Table>() else {
            continue;
        };
        if let Some(pointer) = document
            .get("package")
            .and_then(|value| value.get("workspace"))
            .and_then(toml::Value::as_str)
        {
            if fs::canonicalize(project.root.join(pointer)).ok().as_deref()
                == Some(workspace_root.root.as_path())
            {
                owners.insert(project.id);
            }
        }
    }

    let mut visited = BTreeSet::new();
    while let Some(id) = owners.iter().find(|id| !visited.contains(*id)).copied() {
        visited.insert(id);
        let project = projects
            .get(id)
            .ok_or("workspace member was not discovered")?;
        let document = read_manifest(project)?;
        for dependency in manifest_path_dependencies(&document)? {
            let dependency = project.root.join(dependency);
            let dependency = fs::canonicalize(&dependency).map_err(|error| {
                format!(
                    "cannot verify path dependency {}: {error}",
                    dependency.display()
                )
            })?;
            let Some(relative) = slash_relative_path(&workspace_root.root, &dependency) else {
                continue;
            };
            if spec.is_excluded(&relative) {
                continue;
            }
            let member = find_cargo_project(projects, &dependency).ok_or_else(|| {
                format!(
                    "path dependency was not discovered: {}",
                    strip_verbatim(&dependency).display()
                )
            })?;
            owners.insert(member.id);
        }
    }
    Ok(owners.into_iter().collect())
}

fn find_cargo_project<'a>(projects: &'a [Project], root: &Path) -> Option<&'a Project> {
    projects
        .iter()
        .find(|project| project.root == root && project.has_kind(ProjectKind::Cargo))
}

fn manifest_path_dependencies(document: &toml::Table) -> Result<Vec<&str>, String> {
    fn collect<'a>(table: &'a toml::Table, paths: &mut Vec<&'a str>) -> Result<(), String> {
        for name in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(value) = table.get(name) {
                let dependencies = value.as_table().ok_or("dependencies must be a table")?;
                for value in dependencies.values() {
                    if let Some(path) = value.as_table().and_then(|value| value.get("path")) {
                        paths.push(path.as_str().ok_or("dependency path must be a string")?);
                    }
                }
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    collect(document, &mut paths)?;
    if let Some(targets) = document.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            collect(target, &mut paths)?;
        }
    }
    if let Some(workspace) = document.get("workspace").and_then(toml::Value::as_table) {
        collect(workspace, &mut paths)?;
    }
    Ok(paths)
}

fn is_tauri_project(project: &Project) -> bool {
    ["tauri.conf.json", "tauri.conf.json5", "Tauri.toml"]
        .iter()
        .any(|name| project.root.join(name).is_file())
}

fn nearest_javascript_owner<'a>(path: &Path, projects: &'a [Project]) -> Option<&'a Project> {
    projects
        .iter()
        .filter(|project| {
            project.has_kind(ProjectKind::JavaScript) && is_strict_descendant(&project.root, path)
        })
        .max_by_key(|project| project.root.components().count())
}

/// Returns `Some(is_link)` for an existing directory or link at `path`.
fn existing_directory(path: &Path, errors: &mut Vec<RunError>) -> Option<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Some(false),
        Ok(metadata) if metadata.file_type().is_symlink() => Some(true),
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            errors.push(run_error(path, error));
            None
        }
    }
}

fn make_target(
    path: &Path,
    kind: CleanupKind,
    owners: Vec<ProjectId>,
    is_link: bool,
    ownership_error: Option<String>,
) -> CleanupTarget {
    CleanupTarget {
        path: path.to_path_buf(),
        canonical_path: fs::canonicalize(path).ok(),
        kind,
        owners,
        is_link,
        ownership_error,
    }
}

pub fn execute_plan(plan: &CleanupPlan, dry_run: bool) -> RunReport {
    RunReport {
        root: plan.canonical_root.clone(),
        projects: project_reports(plan),
        targets: plan
            .targets
            .iter()
            .map(|target| execute_target(plan, target, dry_run))
            .collect(),
        errors: plan.errors.clone(),
    }
}

fn project_reports(plan: &CleanupPlan) -> Vec<ProjectReport> {
    plan.projects
        .iter()
        .filter_map(|project| {
            plan.statuses.get(&project.id).map(|status| ProjectReport {
                project: project.clone(),
                status: status.clone(),
            })
        })
        .collect()
}

fn execute_target(plan: &CleanupPlan, original: &CleanupTarget, dry_run: bool) -> TargetReport {
    // Cheap checks against the plan first, so protected targets need no rescan.
    if let Some(outcome) = ownership_protection(plan, original, &plan.canonical_root) {
        return TargetReport::new(original.clone(), outcome);
    }

    let mut target = original.clone();
    if !dry_run {
        // Scanning a large ROOT takes time. Right before deleting, rescan the
        // target's owners, but never add targets to the original deletion set.
        let current_plan = match refresh_for_target(plan, original) {
            Ok(current_plan) => current_plan,
            Err(message) => {
                return TargetReport::new(
                    target,
                    TargetOutcome::Unverified(format!(
                        "cannot re-check before deleting: {message}"
                    )),
                )
            }
        };
        let Some(current) = current_plan
            .targets
            .iter()
            .find(|current| current.path == original.path && current.kind == original.kind)
        else {
            return TargetReport::new(
                target,
                TargetOutcome::Protected("target is no longer in the current plan".into()),
            );
        };
        if current.canonical_path != original.canonical_path {
            return TargetReport::new(
                target,
                TargetOutcome::Failed("target changed since planning".into()),
            );
        }
        // Report the original target: its owner ids refer to `plan`, while
        // ids in the refreshed plan are local to it.
        if let Some(outcome) = ownership_protection(&current_plan, current, &plan.canonical_root) {
            return TargetReport::new(target, outcome);
        }
    }

    if let Some(outcome) = content_protection(&target) {
        return TargetReport::new(target, outcome);
    }
    if let Err(message) = validate_target(&plan.canonical_root, &mut target) {
        return TargetReport::new(target, TargetOutcome::Failed(message));
    }

    let estimated_bytes = size::estimate_directory_size(&target.path).ok();
    if dry_run {
        return TargetReport {
            target,
            outcome: TargetOutcome::WouldDelete,
            estimated_bytes,
        };
    }
    match remove_target(&target.path) {
        Ok(()) => TargetReport {
            target,
            outcome: TargetOutcome::Deleted,
            estimated_bytes,
        },
        Err(message) => TargetReport::new(target, TargetOutcome::Failed(message)),
    }
}

/// Rescans only the subtree of the target's outermost owner. Ownership depends
/// on nothing else: workspace members and nested projects live below it.
fn refresh_for_target(plan: &CleanupPlan, target: &CleanupTarget) -> Result<CleanupPlan, String> {
    let root = target
        .owners
        .iter()
        .filter_map(|id| plan.projects.get(*id))
        .map(|project| project.root.as_path())
        .min_by_key(|root| root.components().count())
        .ok_or("target has no owners")?;
    let options = ScanOptions {
        excludes: plan
            .options
            .excludes
            .iter()
            .filter(|exclude| is_strict_descendant(root, exclude))
            .cloned()
            .collect(),
        ..plan.options.clone()
    };
    build_cleanup_plan(root, &options).map_err(|error| error.to_string())
}

/// Protection that follows from the plan: links, ownership and owner activity.
fn ownership_protection(
    plan: &CleanupPlan,
    target: &CleanupTarget,
    display_root: &Path,
) -> Option<TargetOutcome> {
    if target.is_link {
        return Some(TargetOutcome::Protected(
            "target is a symlink or junction; links are never removed".into(),
        ));
    }
    if let Some(error) = &target.ownership_error {
        return Some(TargetOutcome::Unverified(format!(
            "ownership is unverified: {error}"
        )));
    }
    owner_protection_reason(plan, target, display_root).map(TargetOutcome::Protected)
}

/// Protection that follows from what is inside the directory.
fn content_protection(target: &CleanupTarget) -> Option<TargetOutcome> {
    match has_expected_contents(target) {
        Ok(true) => {}
        Ok(false) => {
            let reason = match target.kind {
                CleanupKind::CargoTarget => {
                    "no Cargo CACHEDIR.TAG, so this may not be build output"
                }
                CleanupKind::NodeModules => "contains no installed packages",
                CleanupKind::YarnCache => "contains no Yarn package archives",
                CleanupKind::YarnUnplugged => "contains no unplugged Yarn packages",
            };
            return Some(TargetOutcome::Protected(reason.into()));
        }
        Err(error) => {
            return Some(TargetOutcome::Unverified(format!(
                "cannot inspect contents: {error}"
            )))
        }
    }
    match tracked_target_reason(&target.path) {
        Ok(Some(reason)) => Some(TargetOutcome::Protected(reason)),
        Err(reason) => Some(TargetOutcome::Unverified(reason)),
        Ok(None) => None,
    }
}

fn owner_protection_reason(
    plan: &CleanupPlan,
    target: &CleanupTarget,
    display_root: &Path,
) -> Option<String> {
    if target.owners.is_empty() {
        return Some("target has no verified owners".into());
    }
    target.owners.iter().find_map(|id| {
        let Some(project) = plan.projects.get(*id) else {
            return Some("an owning project could not be identified".into());
        };
        let name = project.display_name(display_root);
        match plan.statuses.get(id) {
            Some(ProjectStatus::Stale { .. }) => None,
            Some(ProjectStatus::Fresh { latest_activity }) => Some(format!(
                "{name} is active; last activity {}",
                format_relative_time(*latest_activity, plan.options.now)
            )),
            Some(ProjectStatus::Unverified { .. }) => {
                Some(format!("could not verify activity for {name}"))
            }
            None => Some(format!("activity for {name} is unavailable")),
        }
    })
}

/// Checks that a directory holds what its tool creates, so that a folder that
/// merely has the same name (for example `target` with data files) is kept.
fn has_expected_contents(target: &CleanupTarget) -> io::Result<bool> {
    let path = &target.path;
    match target.kind {
        CleanupKind::CargoTarget => match fs::read(path.join("CACHEDIR.TAG")) {
            Ok(content) => Ok(content.starts_with(CARGO_CACHEDIR_SIGNATURE)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        },
        CleanupKind::NodeModules => contains_installed_package(path),
        CleanupKind::YarnCache => any_entry(path, |entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
        }),
        CleanupKind::YarnUnplugged => {
            any_entry(path, |entry| entry.path().join("node_modules").is_dir())
        }
    }
}

/// npm, pnpm, Yarn and bun all leave either a marker or package directories
/// (`name/package.json` or `@scope/name/package.json`).
fn contains_installed_package(node_modules: &Path) -> io::Result<bool> {
    const MARKERS: &[&str] = &[
        ".bin",
        ".bun",
        ".modules.yaml",
        ".package-lock.json",
        ".pnpm",
        ".yarn-integrity",
        ".yarn-state.yml",
    ];
    any_entry(node_modules, |entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if MARKERS.contains(&name.as_ref()) {
            return true;
        }
        let path = entry.path();
        if name.starts_with('@') {
            any_entry(&path, |scoped| scoped.path().join("package.json").is_file()).unwrap_or(false)
        } else {
            path.join("package.json").is_file()
        }
    })
}

fn any_entry(path: &Path, predicate: impl Fn(&fs::DirEntry) -> bool) -> io::Result<bool> {
    for entry in fs::read_dir(path)? {
        if predicate(&entry?) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Moves the directory aside before deleting it. On Windows the rename fails
/// while a file inside is open (a running build or dev server), so a busy
/// directory stays intact instead of being half-deleted.
fn remove_target(path: &Path) -> Result<(), String> {
    let name = path.file_name().ok_or("target has no name")?;
    let parent = path.parent().ok_or("target has no parent")?;
    let staging = parent.join(format!(
        ".{}.project-auto-cleaner-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    fs::rename(path, &staging)
        .map_err(|error| format!("cannot move target aside (is it in use?): {error}"))?;
    fs::remove_dir_all(&staging).map_err(|error| {
        format!(
            "{error}; the partially deleted directory remains at {}",
            strip_verbatim(&staging).display()
        )
    })
}

fn validate_target(canonical_root: &Path, target: &mut CleanupTarget) -> Result<(), String> {
    if !is_strict_descendant(canonical_root, &target.path)
        || target
            .path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("target is not a strict descendant of ROOT".into());
    }
    validate_directory_chain(&target.path)?;
    let canonical_target = fs::canonicalize(&target.path)
        .map_err(|error| format!("cannot canonicalize target: {error}"))?;
    if !is_strict_descendant(canonical_root, &canonical_target) {
        return Err("target is not a strict descendant of ROOT".into());
    }
    if target
        .canonical_path
        .as_ref()
        .is_some_and(|original| original != &canonical_target)
    {
        return Err("target changed since planning".into());
    }
    target.canonical_path = Some(canonical_target);
    Ok(())
}

/// Every component of `path` must be a real directory, not a symlink or
/// junction. Reparse points that do not alias another location (for example
/// cloud-sync placeholders) are regular directories here.
fn validate_directory_chain(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .map_err(|error| format!("cannot inspect {}: {error}", ancestor.display()))?;
        if !metadata.is_dir() {
            return Err(format!(
                "{} is not a regular directory (symlink or junction)",
                strip_verbatim(ancestor).display()
            ));
        }
    }
    Ok(())
}

fn tracked_target_reason(path: &Path) -> Result<Option<String>, String> {
    let parent = path.parent().ok_or("target has no parent")?;
    // Git is optional for plain directories, but an unreadable repository must
    // not result in deleting tracked data (notably Yarn zero-install caches).
    let mut repository = false;
    for ancestor in parent.ancestors() {
        match fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => {
                repository = true;
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot inspect Git repository: {error}")),
        }
    }
    if !repository {
        return Ok(None);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(parent)
        .args(["--literal-pathspecs", "ls-files", "--cached", "-z", "--"])
        .arg(path.file_name().ok_or("target has no name")?)
        .output()
        .map_err(|error| format!("cannot check tracked files: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot check tracked files: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok((!output.stdout.is_empty()).then(|| "contains Git-tracked files".into()))
}

fn dependency_kind(path: &Path) -> Option<CleanupKind> {
    let name = path.file_name()?.to_str()?;
    if name_matches(name, "node_modules") {
        return Some(CleanupKind::NodeModules);
    }
    let parent_is_yarn = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name_matches(name, ".yarn"));
    if parent_is_yarn && name_matches(name, "cache") {
        return Some(CleanupKind::YarnCache);
    }
    if parent_is_yarn && name_matches(name, "unplugged") {
        return Some(CleanupKind::YarnUnplugged);
    }
    None
}

/// Directories that are neither scanned for projects nor counted as activity.
/// `parent_markers` are the project markers of the directory containing `path`.
fn is_skipped_directory(path: &Path, parent_markers: &BTreeMap<ProjectKind, PathBuf>) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if dependency_kind(path).is_some()
        || is_system_directory(path, name)
        || VCS_DIRECTORIES
            .iter()
            .any(|directory| name_matches(name, directory))
    {
        return true;
    }
    !parent_markers.is_empty()
        && (GENERATED_DIRECTORIES
            .iter()
            .any(|directory| name_matches(name, directory))
            || (parent_markers.contains_key(&ProjectKind::Cargo) && name_matches(name, "target")))
}

/// Windows system folders: unreadable for regular users, or installed
/// software. Folders with common names are only skipped at a volume root.
fn is_system_directory(path: &Path, name: &str) -> bool {
    let at_volume_root = path
        .parent()
        .is_some_and(|parent| parent.parent().is_none());
    SYSTEM_DIRECTORIES
        .iter()
        .any(|directory| name_matches(name, directory))
        || (at_volume_root
            && VOLUME_ROOT_SYSTEM_DIRECTORIES
                .iter()
                .any(|directory| name_matches(name, directory)))
}

/// File names are case-insensitive on Windows.
fn name_matches(name: &str, expected: &str) -> bool {
    if cfg!(windows) {
        name.eq_ignore_ascii_case(expected)
    } else {
        name == expected
    }
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

/// Removes the Windows `\\?\` prefix added by `canonicalize` when the path can
/// be written without it (drive letters and UNC shares).
pub fn strip_verbatim(path: &Path) -> PathBuf {
    if cfg!(windows) {
        if let Some(text) = path.to_str() {
            if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{unc}"));
            }
            if let Some(local) = text.strip_prefix(r"\\?\") {
                if local.as_bytes().get(1) == Some(&b':') {
                    return PathBuf::from(local);
                }
            }
        }
    }
    path.to_path_buf()
}

pub fn format_relative_time(time: SystemTime, now: SystemTime) -> String {
    match now.duration_since(time) {
        Ok(duration) if duration.as_secs() == 0 => "just now".into(),
        Ok(duration) => format!("{} ago", format_duration(duration)),
        Err(error) => format!(
            "in {} (future timestamp)",
            format_duration(error.duration())
        ),
    }
}

/// Show at most two units, without approximating calendar months or years.
pub fn format_duration(duration: Duration) -> String {
    let mut remaining = duration.as_secs();
    let mut parts = Vec::new();
    for (seconds, unit) in [
        (SECONDS_PER_DAY, "day"),
        (3_600, "hour"),
        (60, "minute"),
        (1, "second"),
    ] {
        let count = remaining / seconds;
        remaining %= seconds;
        if count > 0 {
            let suffix = if count == 1 { "" } else { "s" };
            parts.push(format!("{count} {unit}{suffix}"));
        }
        if parts.len() == 2 {
            break;
        }
    }
    if parts.is_empty() {
        "less than a second".into()
    } else {
        parts.join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    pub root: PathBuf,
    pub days: u64,
    pub delete: bool,
    pub excludes: Vec<PathBuf>,
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
    S: Into<OsString>,
{
    let mut arguments = args.into_iter().map(Into::into);
    let _program = arguments.next();
    let mut root = None;
    let mut days = DEFAULT_DAYS;
    let mut delete = false;
    let mut dry_run = false;
    let mut excludes = Vec::new();
    let mut verbose = false;
    let mut help = false;
    let mut version = false;
    let mut positional_only = false;

    while let Some(argument) = arguments.next() {
        if positional_only {
            set_root(&mut root, argument)?;
            continue;
        }
        // Paths may be any OS string; options and their inline values are text.
        let Some(text) = argument.to_str().map(str::to_owned) else {
            if argument.as_encoded_bytes().starts_with(b"-") {
                return Err(CliError(
                    "pass non-Unicode option values as a separate argument, e.g. -x PATH".into(),
                ));
            }
            set_root(&mut root, argument)?;
            continue;
        };
        if let Some((name, value)) = text
            .split_once('=')
            .filter(|(name, _)| name.starts_with("--"))
        {
            match name {
                "--days" => days = parse_days(value)?,
                "--exclude" => excludes.push(PathBuf::from(value)),
                _ => return Err(CliError(format!("unknown option: {name}"))),
            }
            continue;
        }
        match text.as_str() {
            "--" => positional_only = true,
            "--delete" => delete = true,
            "-n" | "--dry-run" => dry_run = true,
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => help = true,
            "-V" | "--version" => version = true,
            "-d" | "--days" => {
                let value = option_value(&mut arguments, "--days")?;
                days = parse_days(value.to_str().unwrap_or_default())?;
            }
            "-x" | "--exclude" => {
                excludes.push(PathBuf::from(option_value(&mut arguments, "--exclude")?));
            }
            value if value.starts_with('-') => {
                return Err(CliError(format!("unknown option: {value}")));
            }
            _ => set_root(&mut root, argument)?,
        }
    }

    if delete && dry_run {
        return Err(CliError(
            "--delete and --dry-run cannot be used together".into(),
        ));
    }
    let root = match root {
        Some(root) => root,
        None if help || version => PathBuf::new(),
        None => return Err(CliError("ROOT is required".into())),
    };
    Ok(CliOptions {
        root,
        days,
        delete,
        excludes,
        verbose,
        help,
        version,
    })
}

fn option_value(
    arguments: &mut impl Iterator<Item = OsString>,
    name: &str,
) -> Result<OsString, CliError> {
    arguments
        .next()
        .ok_or_else(|| CliError(format!("{name} requires a value")))
}

fn set_root(root: &mut Option<PathBuf>, value: OsString) -> Result<(), CliError> {
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
    threshold_for_days(days)?;
    Ok(days)
}

pub fn threshold_for_days(days: u64) -> Result<Duration, CliError> {
    if days == 0 {
        return Err(CliError("--days requires a positive integer".into()));
    }
    days.checked_mul(SECONDS_PER_DAY)
        .map(Duration::from_secs)
        .ok_or_else(|| CliError("--days value is too large".into()))
}

pub fn usage() -> &'static str {
    "Remove build and dependency directories from inactive Rust and JavaScript projects.

Usage: project-auto-cleaner [OPTIONS] <ROOT>

Nothing is deleted unless --delete is given; by default the plan is only printed.

Options:
  -d, --days <N>        A project is inactive when no source file changed for more
                        than N days [default: 90]
      --delete          Permanently delete the planned directories
  -n, --dry-run         Only print the plan (the default)
  -x, --exclude <PATH>  Do not scan this directory; relative to ROOT; repeatable
  -v, --verbose         Also show active projects, protected targets and owners
  -h, --help            Print help
  -V, --version         Print version

Cleanup targets: target (Cargo, only with its CACHEDIR.TAG), node_modules,
.yarn/cache and .yarn/unplugged.

Exit codes:
  0  success
  1  something could not be verified, or a deletion failed
  2  invalid arguments or ROOT"
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

    fn set_modified(path: &Path, modified: SystemTime) {
        let file = File::options().write(true).open(path).unwrap();
        let times = fs::FileTimes::new().set_modified(modified);
        file.set_times(times).unwrap();
    }

    fn old_time() -> SystemTime {
        SystemTime::now() - Duration::from_secs(180 * SECONDS_PER_DAY)
    }

    fn scan_options() -> ScanOptions {
        ScanOptions {
            now: SystemTime::now(),
            threshold: Duration::from_secs(90 * SECONDS_PER_DAY),
            excludes: Vec::new(),
        }
    }

    fn plan_for(root: &Path) -> CleanupPlan {
        build_cleanup_plan(root, &scan_options()).unwrap()
    }

    fn old_cargo(root: &Path, content: &str) {
        write_file(&root.join("Cargo.toml"), content);
        set_modified(&root.join("Cargo.toml"), old_time());
    }

    fn old_package(root: &Path) {
        write_file(&root.join("package.json"), "{}");
        set_modified(&root.join("package.json"), old_time());
    }

    /// A target directory as Cargo creates it.
    fn cargo_target(project: &Path) {
        write_file(
            &project.join("target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        write_file(&project.join("target/debug/artifact"), "artifact");
    }

    fn directory_link(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let command = format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                link.display().to_string().replace('\'', "''"),
                target.display().to_string().replace('\'', "''")
            );
            assert!(Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &command])
                .status()
                .unwrap()
                .success());
        }
    }

    fn remove_link(link: &Path) {
        fs::remove_file(link)
            .or_else(|_| fs::remove_dir(link))
            .unwrap();
    }

    fn reason(outcome: &TargetOutcome) -> &str {
        match outcome {
            TargetOutcome::Protected(reason)
            | TargetOutcome::Unverified(reason)
            | TargetOutcome::Failed(reason) => reason,
            TargetOutcome::WouldDelete | TargetOutcome::Deleted => "",
        }
    }

    #[test]
    fn exact_threshold_is_fresh() {
        let cutoff = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        assert!(!is_before_cutoff(cutoff, cutoff));
        assert!(is_before_cutoff(cutoff - Duration::from_secs(1), cutoff));
    }

    #[test]
    fn nested_project_does_not_make_parent_fresh() {
        let temp = TestDir::new();
        let root = temp.path.as_path();
        old_cargo(root, "[package]\nname='old'\nversion='0.1.0'\n");
        write_file(&root.join("src/lib.rs"), "old");
        set_modified(&root.join("src/lib.rs"), old_time());
        write_file(
            &root.join("experiments/package.json"),
            "{\"name\":\"experiment\"}",
        );
        write_file(&root.join("experiments/src/index.js"), "new");

        let mut discovery = DiscoveryState::default();
        walk_discovery(root, &[], &mut discovery);
        let projects = make_projects(&discovery);
        assert_eq!(projects.len(), 2);

        let parent = projects
            .iter()
            .find(|project| project.root == root)
            .unwrap();
        let status = determine_activity(parent, &projects, &scan_options());
        assert!(matches!(status, ProjectStatus::Stale { .. }));
    }

    #[test]
    fn generated_names_below_project_root_count_as_source() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(&temp.path.join("node_modules/dependency/package.json"), "x");
        write_file(&temp.path.join("src/build/recent.js"), "recent source");
        let plan = plan_for(&temp.path);
        assert!(matches!(
            plan.statuses.values().next(),
            Some(ProjectStatus::Fresh { .. })
        ));
        let report = execute_plan(&plan, true);
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::Protected(_)
        ));
    }

    #[test]
    fn generated_output_is_not_a_project_or_source_activity() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(
            &temp.path.join("node_modules/dependency/package.json"),
            "artifact",
        );
        write_file(&temp.path.join(".output/server/package.json"), "{}");
        write_file(
            &temp
                .path
                .join(".output/server/node_modules/dependency/package.json"),
            "generated output",
        );
        let generated_name = if cfg!(windows) { "Dist" } else { "dist" };
        write_file(
            &temp.path.join(generated_name).join("bundle.js"),
            "generated output",
        );
        let plan = plan_for(&temp.path);
        assert_eq!(plan.projects.len(), 1);
        assert_eq!(plan.targets.len(), 1);
        assert!(plan.statuses.values().all(ProjectStatus::is_stale));
        let report = execute_plan(&plan, false);
        assert!(!report.has_errors(), "{report:#?}");
        assert!(!temp.path.join("node_modules").exists());
        assert!(temp
            .path
            .join(".output/server/node_modules/dependency/package.json")
            .exists());
    }

    #[test]
    fn cli_parses_options() {
        let options = parse_cli([
            "project-auto-cleaner",
            "projects",
            "--days=30",
            "-v",
            "-x",
            "archive",
            "--exclude=old",
        ])
        .unwrap();
        assert_eq!(options.root, PathBuf::from("projects"));
        assert_eq!(options.days, 30);
        assert!(!options.delete);
        assert!(options.verbose);
        assert_eq!(
            options.excludes,
            [PathBuf::from("archive"), PathBuf::from("old")]
        );

        let options = parse_cli(["cleaner", "--delete", "-d", "7", "--", "-root"]).unwrap();
        assert!(options.delete);
        assert_eq!(options.days, 7);
        assert_eq!(options.root, PathBuf::from("-root"));

        assert!(parse_cli(["cleaner", "-V"]).unwrap().version);
        assert!(parse_cli(["cleaner", "root", "-n", "--delete"]).is_err());
        assert!(parse_cli(["cleaner", "root", "--unknown"]).is_err());
        assert!(parse_cli(["cleaner", "root", "--unknown=1"]).is_err());
        assert!(parse_cli(["cleaner", "a", "b"]).is_err());
        assert!(parse_cli(["cleaner", "root", "--exclude"]).is_err());
        assert!(parse_cli(["cleaner"]).is_err());
    }

    #[test]
    fn zero_and_overflowing_thresholds_are_rejected() {
        assert!(threshold_for_days(0).is_err());
        assert!(threshold_for_days(u64::MAX).is_err());
        assert!(parse_cli(["cleaner", "root", "--days", "0"]).is_err());
        assert!(parse_cli(["cleaner", "root", "--days", &u64::MAX.to_string()]).is_err());
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(
            format!("[{:14}]", CleanupKind::CargoTarget),
            "[target        ]"
        );
        assert_eq!(format!("[{:6}]", ProjectKind::Cargo), "[cargo ]");
        let root = Path::new("projects");
        let project = |path: PathBuf| Project {
            id: 0,
            root: path,
            markers: BTreeMap::new(),
        };
        assert_eq!(project(root.to_path_buf()).display_name(root), "projects");
        assert_eq!(
            project(root.join("one").join("frontend")).display_name(root),
            "one/frontend"
        );
    }

    #[test]
    fn relative_time_handles_boundaries_and_future_timestamps() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * SECONDS_PER_DAY);
        for (seconds, expected) in [
            (0, "just now"),
            (1, "1 second ago"),
            (59, "59 seconds ago"),
            (60, "1 minute ago"),
            (3_599, "59 minutes 59 seconds ago"),
            (3_600, "1 hour ago"),
            (86_400, "1 day ago"),
            (183_660, "2 days 3 hours ago"),
        ] {
            assert_eq!(
                format_relative_time(now - Duration::from_secs(seconds), now),
                expected
            );
        }
        assert_eq!(
            format_relative_time(now + Duration::from_secs(3_600), now),
            "in 1 hour (future timestamp)"
        );
        assert_eq!(
            format_relative_time(
                SystemTime::UNIX_EPOCH - Duration::from_secs(60),
                SystemTime::UNIX_EPOCH
            ),
            "1 minute ago"
        );
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefix_is_stripped_only_when_safe() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\x")), Path::new(r"C:\x"));
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\x")),
            Path::new(r"\\server\share\x")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\Volume{1}\x")),
            Path::new(r"\\?\Volume{1}\x")
        );
    }

    #[test]
    fn workspace_members_are_resolved_without_unrelated_projects() {
        let temp = TestDir::new();
        let root = fs::canonicalize(&temp.path).unwrap();
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
        write_file(&root.join("crates/README.md"), "not a member");
        write_file(
            &root.join("experiments/test/Cargo.toml"),
            "[package]\nname='test'\n",
        );

        let mut discovery = DiscoveryState::default();
        walk_discovery(&root, &[], &mut discovery);
        let projects = make_projects(&discovery);
        let workspace = projects
            .iter()
            .find(|project| project.root == root)
            .unwrap();
        let CargoOwnership::Workspace(spec) = cargo_ownership(workspace) else {
            panic!("expected workspace");
        };
        let owners = workspace_owners(workspace, &projects, &spec).unwrap();
        let owner_roots = owners
            .iter()
            .map(|id| projects[*id].root.clone())
            .collect::<Vec<_>>();
        assert!(owner_roots.contains(&root));
        assert!(owner_roots.contains(&root.join("crates").join("foo")));
        assert!(!owner_roots.contains(&root.join("crates").join("excluded")));
        assert!(!owner_roots.contains(&root.join("experiments").join("test")));
    }

    #[test]
    fn toml_workspace_syntax_and_globs_are_respected() {
        for content in [
            "[\"workspace\"]\nmembers = [\n 'crates/[ab]',\n]\n",
            "workspace = { members = ['crates/[ab]'] }",
            "workspace.members = ['crates/[ab]']",
        ] {
            let spec = parse_workspace_spec(content).unwrap().unwrap();
            let matches = |path: &str| {
                spec.members
                    .iter()
                    .any(|p| p.matches_with(path, glob_options()))
            };
            assert!(matches("crates/a"));
            assert!(!matches("crates/c"));
            assert!(!matches("crates/a/nested"));
        }
        assert!(parse_workspace_spec("[workspace]\nmembers=['../outside']").is_err());
        assert!(parse_workspace_spec("[workspace]\nmembers=['/rooted']").is_err());
        assert!(parse_workspace_spec("[workspace]\nmembers=['a',,'b']").is_err());
        assert!(parse_workspace_spec("[workspace]\nmembers=['[broken']").is_err());
        assert!(parse_workspace_spec("[workspace]\nworkspace='unterminated").is_err());
    }

    #[test]
    fn workspace_exclude_uses_cargo_prefix_semantics() {
        let spec = parse_workspace_spec(
            "[workspace]\nmembers=['crates/*/*', 'crates/keep/x']\n\
             exclude=['crates/legacy', 'crates/keep', '../outside', 'crates/[ab]']",
        )
        .unwrap()
        .unwrap();
        assert!(spec.is_excluded("crates/legacy/x"));
        assert!(spec.is_excluded("crates/legacy"));
        assert!(!spec.is_excluded("crates/legacy-new/x"));
        assert!(!spec.is_excluded("crates/keep/x"));
        assert!(spec.is_excluded("crates/keep/y"));
        assert!(!spec.is_excluded("crates/a/x"));
    }

    #[test]
    fn stale_cargo_target_is_deleted_but_manifest_is_preserved() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[package]\nname='old'\nversion='0.1.0'\n");
        cargo_target(&temp.path);

        let report = execute_plan(&plan_for(&temp.path), false);

        assert!(!report.has_errors(), "{report:#?}");
        assert!(matches!(report.targets[0].outcome, TargetOutcome::Deleted));
        assert!(report.targets[0]
            .estimated_bytes
            .is_some_and(|bytes| bytes > 0));
        assert!(!temp.path.join("target").exists());
        assert!(temp.path.join("Cargo.toml").exists());
    }

    #[test]
    fn cargo_target_without_cachedir_tag_is_protected() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[package]\nname='old'");
        write_file(&temp.path.join("target/data.csv"), "not build output");
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(!report.has_errors(), "{report:#?}");
        assert!(reason(&report.targets[0].outcome).contains("CACHEDIR.TAG"));
        assert!(temp.path.join("target/data.csv").exists());
    }

    #[test]
    fn dry_run_reports_size_and_keeps_files() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[package]\nname='old'");
        cargo_target(&temp.path);
        let report = execute_plan(&plan_for(&temp.path), true);
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::WouldDelete
        ));
        assert!(report.targets[0]
            .estimated_bytes
            .is_some_and(|bytes| bytes > 0));
        assert!(temp.path.join("target/debug/artifact").exists());
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
            is_link: false,
            ownership_error: None,
        };
        assert!(validate_target(&root, &mut target).is_err());
    }

    #[test]
    fn implicit_path_member_protects_workspace_target() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[workspace]\nmembers=['app']");
        old_cargo(
            &temp.path.join("app"),
            "[package]\nname='app'\n[dependencies]\nlib={path='../lib'}",
        );
        write_file(&temp.path.join("lib/Cargo.toml"), "[package]\nname='lib'");
        cargo_target(&temp.path);
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(!report.has_errors(), "{report:#?}");
        assert!(reason(&report.targets[0].outcome).contains("lib is active"));
        assert!(temp.path.join("target/debug/artifact").exists());
    }

    #[test]
    fn missing_workspace_member_protects_target_and_reports_error() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[workspace]\nmembers=['missing']");
        cargo_target(&temp.path);
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(report.has_errors());
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::Unverified(_)
        ));
        assert!(temp.path.join("target/debug/artifact").exists());
    }

    #[test]
    fn excluded_member_does_not_protect_workspace_target() {
        let temp = TestDir::new();
        old_cargo(
            &temp.path,
            "[workspace]\nmembers=['crates/*']\nexclude=['crates/excluded']",
        );
        old_cargo(&temp.path.join("crates/old"), "[package]\nname='old'");
        write_file(
            &temp.path.join("crates/excluded/Cargo.toml"),
            "[package]\nname='excluded'",
        );
        cargo_target(&temp.path);
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(!report.has_errors(), "{report:#?}");
        assert!(matches!(report.targets[0].outcome, TargetOutcome::Deleted));
    }

    #[test]
    fn nested_js_with_own_dependencies_still_protects_parent() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(&temp.path.join("node_modules/shared/package.json"), "keep");
        write_file(&temp.path.join("packages/app/package.json"), "{}");
        write_file(
            &temp
                .path
                .join("packages/app/node_modules/local/package.json"),
            "keep",
        );
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(!report.has_errors(), "{report:#?}");
        assert_eq!(report.targets.len(), 2);
        assert!(report
            .targets
            .iter()
            .all(|target| matches!(target.outcome, TargetOutcome::Protected(_))));
        assert!(temp.path.join("node_modules/shared/package.json").exists());
    }

    #[test]
    fn nested_cargo_project_protects_parent_node_modules() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(&temp.path.join("node_modules/shared/package.json"), "keep");
        write_file(
            &temp.path.join("src-tauri/Cargo.toml"),
            "[package]\nname='app'",
        );
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(!report.has_errors(), "{report:#?}");
        assert!(reason(&report.targets[0].outcome).contains("src-tauri is active"));
        assert!(temp.path.join("node_modules/shared/package.json").exists());
    }

    #[test]
    fn yarn_artifacts_do_not_count_as_source_activity() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(
            &temp.path.join(".yarn/cache/archive.zip"),
            "recent artifact",
        );
        write_file(
            &temp
                .path
                .join(".yarn/unplugged/package/node_modules/package/index.js"),
            "recent artifact",
        );
        let report = execute_plan(&plan_for(&temp.path), true);
        assert!(!report.has_errors(), "{report:#?}");
        assert_eq!(report.targets.len(), 2);
        assert!(report
            .targets
            .iter()
            .all(|target| matches!(target.outcome, TargetOutcome::WouldDelete)));
        assert!(temp.path.join(".yarn/cache/archive.zip").exists());
    }

    #[test]
    fn yarn_configuration_counts_as_source_activity() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(&temp.path.join(".yarn/plugins/plugin.cjs"), "recent source");
        write_file(&temp.path.join(".yarn/cache/archive.zip"), "artifact");
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::Protected(_)
        ));
    }

    #[test]
    fn source_changes_after_planning_prevent_deletion() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[package]\nname='old'");
        cargo_target(&temp.path);
        let plan = plan_for(&temp.path);
        assert!(plan.statuses.values().all(ProjectStatus::is_stale));
        write_file(&temp.path.join("src/main.rs"), "recent source");
        let report = execute_plan(&plan, false);
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::Protected(_)
        ));
        assert!(temp.path.join("target/debug/artifact").exists());
    }

    #[test]
    fn new_nested_project_after_planning_protects_shared_dependencies() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(&temp.path.join("node_modules/shared/package.json"), "keep");
        let plan = plan_for(&temp.path);
        write_file(&temp.path.join("packages/app/package.json"), "{}");
        let report = execute_plan(&plan, false);
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::Protected(_)
        ));
        assert!(temp.path.join("node_modules/shared/package.json").exists());
    }

    #[test]
    fn unverified_activity_is_an_error_even_without_targets() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[package]\nname='old'");
        let mut plan = plan_for(&temp.path);
        let id = plan.projects[0].id;
        plan.statuses.insert(
            id,
            ProjectStatus::Unverified {
                errors: vec![ActivityError {
                    path: temp.path.clone(),
                    message: "unreadable source".into(),
                }],
            },
        );
        assert!(execute_plan(&plan, true).has_errors());
    }

    #[test]
    fn discovery_errors_only_block_related_targets() {
        let root = Path::new("root");
        let owner = root.join("app");
        let target = owner.join("node_modules");
        let error = |path: PathBuf| RunError {
            path: Some(path),
            message: "access denied".into(),
        };
        let owners = [owner.as_path()];
        assert!(
            incomplete_discovery_reason(&target, &owners, &[error(root.join("other"))]).is_none()
        );
        assert!(
            incomplete_discovery_reason(&target, &owners, &[error(owner.join("packages"))])
                .is_some()
        );
        assert!(incomplete_discovery_reason(&target, &owners, &[error(target.clone())]).is_some());
        let no_path = RunError {
            path: None,
            message: "unknown".into(),
        };
        assert!(incomplete_discovery_reason(&target, &owners, &[no_path]).is_some());
    }

    #[test]
    fn excluded_directories_are_not_scanned() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(&temp.path.join("node_modules/shared/package.json"), "x");
        write_file(&temp.path.join("archive/app/package.json"), "{}");

        let report = execute_plan(&plan_for(&temp.path), true);
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::Protected(_)
        ));

        let options = ScanOptions {
            excludes: vec![PathBuf::from("archive")],
            ..scan_options()
        };
        let plan = build_cleanup_plan(&temp.path, &options).unwrap();
        assert_eq!(plan.projects.len(), 1);
        let report = execute_plan(&plan, true);
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::WouldDelete
        ));

        if cfg!(windows) {
            let options = ScanOptions {
                excludes: vec![PathBuf::from("ARCHIVE")],
                ..scan_options()
            };
            let plan = build_cleanup_plan(&temp.path, &options).unwrap();
            assert_eq!(plan.projects.len(), 1);
        }

        let outside = ScanOptions {
            excludes: vec![temp.path.parent().unwrap().to_path_buf()],
            ..scan_options()
        };
        assert!(build_cleanup_plan(&temp.path, &outside).is_err());
    }

    #[test]
    fn windows_system_directories_are_skipped() {
        let temp = TestDir::new();
        write_file(&temp.path.join("$RECYCLE.BIN/old/package.json"), "{}");
        write_file(
            &temp.path.join("System Volume Information/package.json"),
            "{}",
        );
        write_file(&temp.path.join("Recovery/package.json"), "{}");
        let plan = plan_for(&temp.path);
        let roots = plan
            .projects
            .iter()
            .map(|project| project.display_name(&plan.canonical_root))
            .collect::<Vec<_>>();
        assert_eq!(roots, ["Recovery"]);

        let empty = BTreeMap::new();
        let volume_root = Path::new(if cfg!(windows) { r"E:\" } else { "/" });
        for name in ["Recovery", "Program Files", "Windows"] {
            assert!(is_skipped_directory(&volume_root.join(name), &empty));
            assert!(!is_skipped_directory(&temp.path.join(name), &empty));
        }
    }

    #[test]
    fn disk_usage_counts_hard_links_once() {
        let temp = TestDir::new();
        let outside = TestDir::new();
        let directory = temp.path.join("target");
        write_file(&directory.join("a.bin"), &"x".repeat(64 * 1024));
        let single = size::estimate_directory_size(&directory).unwrap();
        assert!(single >= 64 * 1024, "{single}");

        fs::hard_link(directory.join("a.bin"), directory.join("b.bin")).unwrap();
        assert_eq!(size::estimate_directory_size(&directory).unwrap(), single);

        // Deleting a file that is also linked elsewhere frees nothing.
        write_file(&directory.join("c.bin"), &"y".repeat(64 * 1024));
        fs::hard_link(directory.join("c.bin"), outside.path.join("c.bin")).unwrap();
        assert_eq!(size::estimate_directory_size(&directory).unwrap(), single);
    }

    #[cfg(unix)]
    #[test]
    fn size_estimate_uses_allocated_blocks_without_following_symlinks_or_external_hard_links() {
        use std::os::unix::fs::{symlink, MetadataExt};
        let temp = TestDir::new();
        let target = temp.path.join("target");
        fs::create_dir(&target).unwrap();
        write_file(&target.join("artifact"), &"x".repeat(8_192));
        fs::hard_link(target.join("artifact"), target.join("artifact-link")).unwrap();
        write_file(&temp.path.join("outside"), &"x".repeat(16_384));
        fs::hard_link(temp.path.join("outside"), target.join("shared")).unwrap();
        symlink(temp.path.join("outside"), target.join("symlink")).unwrap();
        let sparse = File::create(target.join("sparse")).unwrap();
        sparse.set_len(1_000_000_000).unwrap();

        let expected = [
            &target,
            &target.join("artifact"),
            &target.join("symlink"),
            &target.join("sparse"),
        ]
        .iter()
        .map(|path| fs::symlink_metadata(path).unwrap().blocks() * 512)
        .sum::<u64>();
        assert_eq!(size::estimate_directory_size(&target).unwrap(), expected);
    }

    #[test]
    fn fresh_workspace_member_protects_shared_target_with_a_readable_reason() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[workspace]\nmembers=['crates/*']\n");
        write_file(
            &temp.path.join("crates/api/Cargo.toml"),
            "[package]\nname='api'\n",
        );
        cargo_target(&temp.path);
        let report = execute_plan(&plan_for(&temp.path), false);
        let TargetOutcome::Protected(reason) = &report.targets[0].outcome else {
            panic!("a fresh member must protect its shared target");
        };
        assert!(
            reason.contains("crates/api is active; last activity"),
            "{reason}"
        );
        assert!(!reason.contains("project #"));
        assert!(temp.path.join("target/debug/artifact").exists());
        assert!(report.targets[0].estimated_bytes.is_none());
    }

    #[test]
    fn dependency_directories_must_contain_installed_packages() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(&temp.path.join("node_modules/notes.txt"), "not a package");
        write_file(&temp.path.join(".yarn/cache/notes.txt"), "not an archive");
        let report = execute_plan(&plan_for(&temp.path), true);
        assert!(!report.has_errors(), "{report:#?}");
        assert!(reason(&report.targets[0].outcome).contains("Yarn package archives"));
        assert!(reason(&report.targets[1].outcome).contains("no installed packages"));

        for installed in [
            "@scope/name/package.json",
            ".package-lock.json",
            ".bin/tool",
        ] {
            let temp = TestDir::new();
            old_package(&temp.path);
            write_file(&temp.path.join("node_modules").join(installed), "{}");
            let report = execute_plan(&plan_for(&temp.path), true);
            assert!(
                matches!(report.targets[0].outcome, TargetOutcome::WouldDelete),
                "{installed}: {report:#?}"
            );
        }
    }

    #[test]
    fn tauri_frontend_activity_protects_rust_target() {
        for is_tauri in [true, false] {
            let temp = TestDir::new();
            write_file(&temp.path.join("package.json"), "{}");
            let backend = temp.path.join("src-tauri");
            old_cargo(&backend, "[package]\nname='app'");
            if is_tauri {
                write_file(&backend.join("tauri.conf.json"), "{}");
                set_modified(&backend.join("tauri.conf.json"), old_time());
            }
            cargo_target(&backend);
            let report = execute_plan(&plan_for(&temp.path), false);
            assert!(!report.has_errors(), "{report:#?}");
            let deleted = matches!(report.targets[0].outcome, TargetOutcome::Deleted);
            assert_eq!(deleted, !is_tauri, "{report:#?}");
        }
    }

    #[test]
    fn deleted_target_keeps_owner_ids_of_the_plan() {
        let temp = TestDir::new();
        write_file(&temp.path.join("a-active/package.json"), "{}");
        old_cargo(&temp.path.join("b-old"), "[package]\nname='old'");
        cargo_target(&temp.path.join("b-old"));
        let plan = plan_for(&temp.path);
        let owner = plan
            .projects
            .iter()
            .find(|project| project.root.ends_with("b-old"))
            .unwrap()
            .id;
        assert_ne!(owner, 0);
        let report = execute_plan(&plan, false);
        assert!(matches!(report.targets[0].outcome, TargetOutcome::Deleted));
        assert_eq!(report.targets[0].target.owners, [owner]);
    }

    #[test]
    fn deletion_leaves_no_staging_directory() {
        let temp = TestDir::new();
        old_cargo(&temp.path, "[package]\nname='old'");
        cargo_target(&temp.path);
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(matches!(report.targets[0].outcome, TargetOutcome::Deleted));
        let leftovers = fs::read_dir(&temp.path)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("project-auto-cleaner")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[cfg(windows)]
    #[test]
    fn directory_in_use_is_left_intact() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let temp = TestDir::new();
        old_cargo(&temp.path, "[package]\nname='old'");
        cargo_target(&temp.path);
        write_file(&temp.path.join("target/debug/second"), "artifact");
        let plan = plan_for(&temp.path);
        let open = File::options()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(temp.path.join("target/debug/artifact"))
            .unwrap();
        let report = execute_plan(&plan, false);
        drop(open);
        assert!(reason(&report.targets[0].outcome).contains("in use"));
        assert!(temp.path.join("target/debug/second").exists());
    }

    fn non_unicode(prefix: &str) -> OsString {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let mut wide = prefix.encode_utf16().collect::<Vec<_>>();
            wide.push(0xD800);
            OsString::from_wide(&wide)
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let mut bytes = prefix.as_bytes().to_vec();
            bytes.push(0xFF);
            OsString::from_vec(bytes)
        }
    }

    #[test]
    fn cli_accepts_non_unicode_paths() {
        let path = non_unicode("projects");
        let options = parse_cli([
            OsString::from("cleaner"),
            path.clone(),
            OsString::from("-x"),
            path.clone(),
        ])
        .unwrap();
        assert_eq!(options.root, PathBuf::from(&path));
        assert_eq!(options.excludes, [PathBuf::from(&path)]);

        let mut inline = OsString::from("--exclude=");
        inline.push(&path);
        assert!(parse_cli([OsString::from("cleaner"), OsString::from("root"), inline]).is_err());
    }

    #[test]
    fn git_tracked_cache_is_preserved() {
        let temp = TestDir::new();
        assert!(Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(&temp.path)
            .status()
            .unwrap()
            .success());
        old_package(&temp.path);
        write_file(
            &temp.path.join(".yarn/cache/archive.zip"),
            "tracked dependency",
        );
        assert!(Command::new("git")
            .arg("-C")
            .arg(&temp.path)
            .args(["add", "--", ".yarn/cache"])
            .status()
            .unwrap()
            .success());
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(!report.has_errors(), "{report:#?}");
        assert!(reason(&report.targets[0].outcome).contains("Git-tracked"));
        assert!(temp.path.join(".yarn/cache/archive.zip").exists());
    }

    #[test]
    fn linked_dependency_directory_is_protected_not_followed() {
        let temp = TestDir::new();
        let external = TestDir::new();
        old_package(&temp.path);
        write_file(&external.path.join("shared/index.js"), "keep");
        directory_link(&external.path, &temp.path.join("node_modules"));
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(!report.has_errors(), "{report:#?}");
        assert!(reason(&report.targets[0].outcome).contains("link"));
        assert!(external.path.join("shared/index.js").exists());
        remove_link(&temp.path.join("node_modules"));
    }

    #[test]
    fn replaced_parent_directory_cannot_redirect_deletion() {
        let temp = TestDir::new();
        let project = temp.path.join("project");
        old_cargo(&project, "[package]\nname='old'");
        cargo_target(&project);
        let plan = plan_for(&temp.path);
        let moved = temp.path.join("moved");
        fs::rename(&project, &moved).unwrap();
        directory_link(&moved, &project);
        let mut target = plan.targets[0].clone();
        assert!(validate_target(&plan.canonical_root, &mut target).is_err());
        let report = execute_plan(&plan, false);
        assert!(!matches!(report.targets[0].outcome, TargetOutcome::Deleted));
        assert!(moved.join("target/debug/artifact").exists());
        remove_link(&project);
    }

    #[test]
    fn links_within_the_project_do_not_block_verification() {
        let temp = TestDir::new();
        old_package(&temp.path);
        write_file(&temp.path.join("node_modules/shared/package.json"), "x");
        write_file(&temp.path.join("cache/pkg@1/index.js"), "old");
        set_modified(&temp.path.join("cache/pkg@1/index.js"), old_time());
        fs::create_dir_all(temp.path.join("cache/pkg")).unwrap();
        directory_link(
            &temp.path.join("cache/pkg@1"),
            &temp.path.join("cache/pkg/1"),
        );
        // Like bun's cache: the link target is itself a nested project.
        write_file(&temp.path.join("cache/nested@1/package.json"), "{}");
        set_modified(&temp.path.join("cache/nested@1/package.json"), old_time());
        directory_link(
            &temp.path.join("cache/nested@1"),
            &temp.path.join("cache/pkg/nested"),
        );

        let report = execute_plan(&plan_for(&temp.path), true);
        assert!(!report.has_errors(), "{report:#?}");
        assert_eq!(report.projects.len(), 2);
        assert!(report
            .projects
            .iter()
            .all(|project| project.status.is_stale()));
        assert!(matches!(
            report.targets[0].outcome,
            TargetOutcome::WouldDelete
        ));
        remove_link(&temp.path.join("cache/pkg/1"));
        remove_link(&temp.path.join("cache/pkg/nested"));
    }

    #[test]
    fn source_directory_link_makes_activity_unverified() {
        let temp = TestDir::new();
        let external = TestDir::new();
        old_cargo(&temp.path, "[package]\nname='old'");
        write_file(&external.path.join("lib.rs"), "new source");
        cargo_target(&temp.path);
        directory_link(&external.path, &temp.path.join("src"));
        let report = execute_plan(&plan_for(&temp.path), false);
        assert!(report.has_errors());
        assert!(temp.path.join("target/debug/artifact").exists());
        assert!(external.path.join("lib.rs").exists());
        remove_link(&temp.path.join("src"));
    }
}
