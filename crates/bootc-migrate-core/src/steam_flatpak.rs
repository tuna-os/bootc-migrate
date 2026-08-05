//! Move a per-user system Steam library into Flatpak Steam's data directory.
//!
//! Steam's client runtime is intentionally left in place. This migrates only
//! the portable state that Flatpak Steam needs to reuse installed games and
//! user settings: `steamapps`, `userdata`, and `config`.

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FLATPAK_STEAM_APP_ID: &str = "com.valvesoftware.Steam";
const PORTABLE_DIRS: &[&str] = &["steamapps", "userdata", "config"];

#[derive(Debug, Clone)]
pub struct SteamFlatpakPlan {
    pub native_root: PathBuf,
    pub flatpak_root: PathBuf,
    pub portable_directories: Vec<String>,
    pub native_library_path: String,
    pub flatpak_library_path: String,
}

#[derive(Debug, Clone)]
pub struct SteamFlatpakOutcome {
    pub plan: SteamFlatpakPlan,
    pub backup_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct SteamPaths {
    native_root: PathBuf,
    flatpak_app_root: PathBuf,
    flatpak_root: PathBuf,
}

impl SteamPaths {
    fn from_home(home: &Path) -> Self {
        let flatpak_app_root = home.join(".var/app").join(FLATPAK_STEAM_APP_ID);
        Self {
            native_root: home.join(".local/share/Steam"),
            flatpak_root: flatpak_app_root.join("data/Steam"),
            flatpak_app_root,
        }
    }

    fn native_component(&self, name: &str) -> PathBuf {
        self.native_root.join(name)
    }

    fn flatpak_component(&self, name: &str) -> PathBuf {
        self.flatpak_root.join(name)
    }
}

#[derive(Debug)]
struct PreparedMigration {
    paths: SteamPaths,
    plan: SteamFlatpakPlan,
    rewritten_libraryfolders: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VdfString {
    value: String,
    content_start: usize,
    content_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VdfToken {
    String(VdfString),
    OpenBrace,
    CloseBrace,
}

#[derive(Debug, Clone)]
struct CompletedMove {
    from: PathBuf,
    to: PathBuf,
}

/// Refuse to migrate while a Steam client or its Flatpak instance is active.
///
/// The check is deliberately narrow: `flatpak ps` checks the current user's
/// Flatpak session, and `pgrep` checks the native client plus its helper.
pub fn ensure_steam_is_stopped() -> Result<()> {
    let flatpak = Command::new("flatpak")
        .args(["ps", "--columns=application"])
        .output()
        .context("checking whether Flatpak Steam is running")?;
    if !flatpak.status.success() {
        bail!(
            "flatpak ps failed while checking whether Steam is running: {}",
            String::from_utf8_lossy(&flatpak.stderr).trim()
        );
    }
    if String::from_utf8_lossy(&flatpak.stdout)
        .lines()
        .map(str::trim)
        .any(|app| app == FLATPAK_STEAM_APP_ID || app.starts_with("com.valvesoftware.Steam."))
    {
        bail!("Flatpak Steam is running; quit Steam and all Steam games before migrating");
    }

    let uid = rustix::process::getuid().as_raw().to_string();
    for process in ["steam", "steamwebhelper"] {
        let status = Command::new("pgrep")
            .args(["-u", &uid, "-x", process])
            .status()
            .with_context(|| format!("checking whether native {process} is running"))?;
        match status.code() {
            Some(0) => {
                bail!(
                    "native {process} is running; quit Steam and all Steam games before migrating"
                );
            }
            Some(1) => {}
            Some(code) => bail!("pgrep failed while checking native {process}: exit status {code}"),
            None => bail!("pgrep was terminated while checking native {process}"),
        }
    }
    Ok(())
}

/// Produce a validated migration plan without changing Steam data.
pub fn plan(home: &Path) -> Result<SteamFlatpakPlan> {
    Ok(prepare(home)?.plan)
}

/// Move system Steam's portable state into Flatpak Steam.
///
/// The operation only uses [`fs::rename`]. A cross-filesystem move is refused
/// rather than falling back to an expensive copy of game files. Existing
/// Flatpak state is moved into a backup directory before the native state is
/// installed, and all completed renames are reversed if a later step fails.
pub fn migrate(home: &Path, dry_run: bool) -> Result<SteamFlatpakOutcome> {
    let prepared = prepare(home)?;
    if dry_run {
        return Ok(SteamFlatpakOutcome {
            plan: prepared.plan,
            backup_dir: None,
        });
    }

    let backup_dir = create_backup_dir(&prepared.paths.flatpak_app_root)?;
    let mut moves = Vec::new();
    let result = perform_migration(&prepared, &backup_dir, &mut moves);
    if let Err(error) = result {
        let rollback = rollback_moves(&moves);
        let cleanup = remove_rewrite_staging_file(&prepared.paths);
        return match (rollback, cleanup) {
            (Ok(()), Ok(())) => Err(error.context(
                "Steam migration failed after staging data; the original layouts were restored",
            )),
            (rollback, cleanup) => {
                let rollback_error = rollback.err().map(|e| format!("{e:#}"));
                let cleanup_error = cleanup.err().map(|e| format!("{e:#}"));
                bail!(
                    "{error:#}; automatic rollback was incomplete (rollback: {}, staging cleanup: {}). \
                     Inspect {} before retrying",
                    rollback_error.as_deref().unwrap_or("ok"),
                    cleanup_error.as_deref().unwrap_or("ok"),
                    backup_dir.display()
                );
            }
        };
    }

    Ok(SteamFlatpakOutcome {
        plan: prepared.plan,
        backup_dir: Some(backup_dir),
    })
}

fn prepare(home: &Path) -> Result<PreparedMigration> {
    let paths = SteamPaths::from_home(home);
    ensure_directory(&paths.native_root, "system Steam root")?;
    ensure_directory(&paths.flatpak_root, "Flatpak Steam root")?;
    ensure_same_filesystem(&paths.native_root, &paths.flatpak_root)?;

    for name in PORTABLE_DIRS {
        ensure_directory(
            &paths.native_component(name),
            &format!("system Steam {name} directory"),
        )?;
        ensure_optional_directory(
            &paths.flatpak_component(name),
            &format!("Flatpak Steam {name} directory"),
        )?;
    }

    let native_vdf = paths
        .native_component("steamapps")
        .join("libraryfolders.vdf");
    let flatpak_vdf = paths
        .flatpak_component("steamapps")
        .join("libraryfolders.vdf");
    ensure_regular_file(&native_vdf, "system Steam library registry")?;
    ensure_regular_file(&flatpak_vdf, "Flatpak Steam library registry")?;

    let native_contents = fs::read_to_string(&native_vdf)
        .with_context(|| format!("reading {}", native_vdf.display()))?;
    let flatpak_contents = fs::read_to_string(&flatpak_vdf)
        .with_context(|| format!("reading {}", flatpak_vdf.display()))?;
    let native_library_path =
        library_path_for_root(&native_contents, &paths.native_root, "system Steam")?;
    let flatpak_library_path =
        library_path_for_root(&flatpak_contents, &paths.flatpak_root, "Flatpak Steam")?;
    let rewritten_libraryfolders = rewrite_library_path(
        &native_contents,
        &native_library_path,
        &flatpak_library_path,
    )?;

    Ok(PreparedMigration {
        plan: SteamFlatpakPlan {
            native_root: paths.native_root.clone(),
            flatpak_root: paths.flatpak_root.clone(),
            portable_directories: PORTABLE_DIRS
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            native_library_path,
            flatpak_library_path,
        },
        paths,
        rewritten_libraryfolders,
    })
}

fn perform_migration(
    prepared: &PreparedMigration,
    backup_dir: &Path,
    moves: &mut Vec<CompletedMove>,
) -> Result<()> {
    let native_backup = backup_dir.join("native");
    let flatpak_backup = backup_dir.join("flatpak");
    fs::create_dir(&native_backup)
        .with_context(|| format!("creating {}", native_backup.display()))?;
    fs::create_dir(&flatpak_backup)
        .with_context(|| format!("creating {}", flatpak_backup.display()))?;

    for name in PORTABLE_DIRS {
        let source = prepared.paths.flatpak_component(name);
        if path_exists(&source)? {
            move_and_record(&source, &flatpak_backup.join(name), moves)?;
        }
    }
    for name in PORTABLE_DIRS {
        move_and_record(
            &prepared.paths.native_component(name),
            &native_backup.join(name),
            moves,
        )?;
    }
    for name in PORTABLE_DIRS {
        move_and_record(
            &native_backup.join(name),
            &prepared.paths.flatpak_component(name),
            moves,
        )?;
    }

    let active_registry = prepared
        .paths
        .flatpak_component("steamapps")
        .join("libraryfolders.vdf");
    let native_registry_backup = backup_dir.join("native-libraryfolders.vdf");
    move_and_record(&active_registry, &native_registry_backup, moves)?;

    let rewrite_staging = rewrite_staging_path(&prepared.paths);
    write_new_file(&rewrite_staging, &prepared.rewritten_libraryfolders)?;
    move_and_record(&rewrite_staging, &active_registry, moves)?;
    Ok(())
}

fn rollback_moves(moves: &[CompletedMove]) -> Result<()> {
    let mut failures = Vec::new();
    for completed in moves.iter().rev() {
        let source_exists = match path_exists(&completed.from) {
            Ok(exists) => exists,
            Err(error) => {
                failures.push(format!(
                    "inspecting {}: {error:#}",
                    completed.from.display()
                ));
                continue;
            }
        };
        let target_exists = match path_exists(&completed.to) {
            Ok(exists) => exists,
            Err(error) => {
                failures.push(format!("inspecting {}: {error:#}", completed.to.display()));
                continue;
            }
        };
        if source_exists || !target_exists {
            failures.push(format!(
                "cannot restore {} from {} (source exists: {source_exists}, staged target exists: {target_exists})",
                completed.from.display(),
                completed.to.display()
            ));
            continue;
        }
        if let Err(error) = fs::rename(&completed.to, &completed.from) {
            failures.push(format!(
                "restoring {} to {}: {error}",
                completed.to.display(),
                completed.from.display()
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

fn move_and_record(from: &Path, to: &Path, moves: &mut Vec<CompletedMove>) -> Result<()> {
    ensure_path_absent(to)?;
    fs::rename(from, to).with_context(|| {
        format!(
            "renaming {} to {} (refusing to copy Steam data across filesystems)",
            from.display(),
            to.display()
        )
    })?;
    moves.push(CompletedMove {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
    });
    Ok(())
}

fn create_backup_dir(flatpak_app_root: &Path) -> Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    for sequence in 0..1000 {
        let suffix = if sequence == 0 {
            timestamp.to_string()
        } else {
            format!("{timestamp}-{sequence}")
        };
        let candidate = flatpak_app_root.join(format!(".bootc-migrate-steam-backup-{suffix}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating Steam backup {}", candidate.display()));
            }
        }
    }
    bail!(
        "could not allocate a unique Steam backup directory below {}",
        flatpak_app_root.display()
    );
}

fn ensure_same_filesystem(native_root: &Path, flatpak_root: &Path) -> Result<()> {
    let native = file_identity(native_root)?;
    let flatpak = file_identity(flatpak_root)?;
    if native.device != flatpak.device {
        bail!(
            "{} and {} are on different filesystems; refusing to copy game data",
            native_root.display(),
            flatpak_root.display()
        );
    }
    Ok(())
}

fn ensure_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {description} at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{description} must be a real directory: {}", path.display());
    }
    Ok(())
}

fn ensure_optional_directory(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("{description} must be a real directory: {}", path.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting {description} at {}", path.display()));
        }
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {description} at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{description} must be a regular file: {}", path.display());
    }
    Ok(())
}

fn ensure_path_absent(path: &Path) -> Result<()> {
    if path_exists(path)? {
        bail!("refusing to overwrite existing path {}", path.display());
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn file_identity(path: &Path) -> Result<FileIdentity> {
    let metadata = fs::metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn library_path_for_root(contents: &str, root: &Path, description: &str) -> Result<String> {
    let root_identity = file_identity(root)?;
    let mut matches = Vec::new();
    for candidate in library_path_values(contents)? {
        let metadata = match fs::metadata(&candidate.value) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspecting {description} library path {}", candidate.value)
                });
            }
        };
        if metadata.dev() == root_identity.device && metadata.ino() == root_identity.inode {
            matches.push(candidate.value);
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!(
            "could not find a {description} library path that resolves to {}",
            root.display()
        ),
        _ => bail!(
            "found multiple {description} library paths that resolve to {}",
            root.display()
        ),
    }
}

fn rewrite_library_path(contents: &str, source_path: &str, target_path: &str) -> Result<String> {
    if target_path.contains(['"', '\\']) {
        bail!("Flatpak Steam library path contains unsupported VDF escaping");
    }

    let matches: Vec<_> = library_path_values(contents)?
        .into_iter()
        .filter(|candidate| candidate.value == source_path)
        .collect();
    if matches.len() != 1 {
        bail!(
            "expected exactly one system Steam library path {:?}, found {}",
            source_path,
            matches.len()
        );
    }

    let replacement = &matches[0];
    let mut rewritten = String::with_capacity(
        contents.len() - (replacement.content_end - replacement.content_start) + target_path.len(),
    );
    rewritten.push_str(&contents[..replacement.content_start]);
    rewritten.push_str(target_path);
    rewritten.push_str(&contents[replacement.content_end..]);
    Ok(rewritten)
}

fn library_path_values(contents: &str) -> Result<Vec<VdfString>> {
    let tokens = tokenize_vdf(contents)?;
    let mut values = Vec::new();
    for pair in tokens.windows(2) {
        if let [VdfToken::String(key), VdfToken::String(value)] = pair
            && key.value == "path"
        {
            values.push(value.clone());
        }
    }
    Ok(values)
}

fn tokenize_vdf(contents: &str) -> Result<Vec<VdfToken>> {
    let bytes = contents.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut depth = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' | b'\r' | b'\n' => index += 1,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'{' => {
                tokens.push(VdfToken::OpenBrace);
                depth += 1;
                index += 1;
            }
            b'}' => {
                if depth == 0 {
                    bail!("unmatched closing brace in Steam library registry");
                }
                tokens.push(VdfToken::CloseBrace);
                depth -= 1;
                index += 1;
            }
            b'"' => {
                let content_start = index + 1;
                index += 1;
                let mut value = String::new();
                let content_end = loop {
                    if index == bytes.len() {
                        bail!("unterminated quoted string in Steam library registry");
                    }
                    match bytes[index] {
                        b'"' => {
                            let end = index;
                            index += 1;
                            break end;
                        }
                        b'\\' => {
                            index += 1;
                            if index == bytes.len() {
                                bail!("unterminated escape in Steam library registry");
                            }
                            let escaped = match bytes[index] {
                                b'"' => '"',
                                b'\\' => '\\',
                                b'n' => '\n',
                                b't' => '\t',
                                byte => {
                                    bail!(
                                        "unsupported escape \\{} in Steam library registry",
                                        byte as char
                                    );
                                }
                            };
                            value.push(escaped);
                            index += 1;
                        }
                        _ => {
                            let character = contents[index..]
                                .chars()
                                .next()
                                .expect("index always points at a UTF-8 boundary");
                            value.push(character);
                            index += character.len_utf8();
                        }
                    }
                };
                tokens.push(VdfToken::String(VdfString {
                    value,
                    content_start,
                    content_end,
                }));
            }
            byte => {
                bail!(
                    "unexpected character {:?} in Steam library registry",
                    byte as char
                );
            }
        }
    }
    if depth != 0 {
        bail!("unclosed brace in Steam library registry");
    }
    Ok(tokens)
}

fn rewrite_staging_path(paths: &SteamPaths) -> PathBuf {
    paths
        .flatpak_component("steamapps")
        .join(".bootc-migrate-libraryfolders.vdf")
}

fn write_new_file(path: &Path, contents: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", path.display()))?;
    Ok(())
}

fn remove_rewrite_staging_file(paths: &SteamPaths) -> Result<()> {
    let path = rewrite_staging_path(paths);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    struct Fixture {
        _temp: TempDir,
        home: PathBuf,
        native_root: PathBuf,
        flatpak_root: PathBuf,
        flatpak_alias: PathBuf,
    }

    fn libraryfolders(path: &Path) -> String {
        format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n}}\n",
            path.display()
        )
    }

    fn fixture() -> Fixture {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let paths = SteamPaths::from_home(&home);
        let flatpak_alias = paths.flatpak_app_root.join(".local/share").join("Steam");

        for name in PORTABLE_DIRS {
            fs::create_dir_all(paths.native_component(name)).unwrap();
            fs::create_dir_all(paths.flatpak_component(name)).unwrap();
            fs::write(
                paths.native_component(name).join("native-marker"),
                format!("native-{name}"),
            )
            .unwrap();
            fs::write(
                paths.flatpak_component(name).join("flatpak-marker"),
                format!("flatpak-{name}"),
            )
            .unwrap();
        }
        fs::create_dir_all(flatpak_alias.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../../data/Steam", &flatpak_alias).unwrap();
        fs::write(
            paths
                .native_component("steamapps")
                .join("libraryfolders.vdf"),
            libraryfolders(&paths.native_root),
        )
        .unwrap();
        fs::write(
            paths
                .flatpak_component("steamapps")
                .join("libraryfolders.vdf"),
            libraryfolders(&flatpak_alias),
        )
        .unwrap();

        Fixture {
            _temp: temp,
            home,
            native_root: paths.native_root,
            flatpak_root: paths.flatpak_root,
            flatpak_alias,
        }
    }

    #[test]
    fn rewrite_library_path_handles_comments_and_escaped_strings() {
        struct Case {
            input: &'static str,
            source: &'static str,
            target: &'static str,
            expected: Option<&'static str>,
        }

        let cases = [
            Case {
                input: "\"libraryfolders\"\n{\n// comment\n\"0\" { \"path\" \"/var/home/test/.local/share/Steam\" }\n}\n",
                source: "/var/home/test/.local/share/Steam",
                target: "/home/test/.var/app/com.valvesoftware.Steam/.local/share/Steam",
                expected: Some(
                    "\"libraryfolders\"\n{\n// comment\n\"0\" { \"path\" \"/home/test/.var/app/com.valvesoftware.Steam/.local/share/Steam\" }\n}\n",
                ),
            },
            Case {
                input: "\"libraryfolders\" { \"0\" { \"path\" \"/one\" } \"1\" { \"path\" \"/two\" } }\n",
                source: "/one",
                target: "/target",
                expected: Some(
                    "\"libraryfolders\" { \"0\" { \"path\" \"/target\" } \"1\" { \"path\" \"/two\" } }\n",
                ),
            },
            Case {
                input: "\"libraryfolders\" { \"0\" { \"path\" \"/other\" } }\n",
                source: "/missing",
                target: "/target",
                expected: None,
            },
            Case {
                input: "\"libraryfolders\" { \"0\" { \"path\" \"/one\" }\n",
                source: "/one",
                target: "/target",
                expected: None,
            },
        ];

        for case in cases {
            assert_eq!(
                rewrite_library_path(case.input, case.source, case.target)
                    .ok()
                    .as_deref(),
                case.expected
            );
        }
    }

    #[test]
    fn migration_dry_run_does_not_touch_steam_data() {
        let fixture = fixture();
        let source_vdf = fixture.native_root.join("steamapps/libraryfolders.vdf");
        let target_vdf = fixture.flatpak_root.join("steamapps/libraryfolders.vdf");
        let source_before = fs::read_to_string(&source_vdf).unwrap();
        let target_before = fs::read_to_string(&target_vdf).unwrap();

        let outcome = migrate(&fixture.home, true).unwrap();

        assert!(outcome.backup_dir.is_none());
        assert!(fixture.native_root.join("steamapps/native-marker").exists());
        assert!(
            fixture
                .flatpak_root
                .join("steamapps/flatpak-marker")
                .exists()
        );
        assert_eq!(fs::read_to_string(source_vdf).unwrap(), source_before);
        assert_eq!(fs::read_to_string(target_vdf).unwrap(), target_before);
    }

    #[test]
    fn migration_renames_portable_state_and_preserves_flatpak_backup() {
        let fixture = fixture();
        let native_steamapps = fixture.native_root.join("steamapps");
        let native_identity = file_identity(&native_steamapps).unwrap();

        let outcome = migrate(&fixture.home, false).unwrap();
        let backup = outcome.backup_dir.unwrap();

        for name in PORTABLE_DIRS {
            assert!(!fixture.native_root.join(name).exists(), "{name} was moved");
            assert!(
                fixture
                    .flatpak_root
                    .join(name)
                    .join("native-marker")
                    .exists(),
                "{name} contains native state"
            );
            assert!(
                backup
                    .join("flatpak")
                    .join(name)
                    .join("flatpak-marker")
                    .exists(),
                "{name} has Flatpak backup"
            );
        }
        assert_eq!(
            file_identity(&fixture.flatpak_root.join("steamapps")).unwrap(),
            native_identity
        );
        assert!(backup.join("native-libraryfolders.vdf").exists());
        let active_registry =
            fs::read_to_string(fixture.flatpak_root.join("steamapps/libraryfolders.vdf")).unwrap();
        assert!(active_registry.contains(&fixture.flatpak_alias.display().to_string()));
        assert!(!active_registry.contains(&fixture.native_root.display().to_string()));
    }

    #[test]
    fn plan_requires_the_flatpak_registry_to_resolve_to_its_data_root() {
        let fixture = fixture();
        fs::write(
            fixture.flatpak_root.join("steamapps/libraryfolders.vdf"),
            libraryfolders(&fixture.native_root),
        )
        .unwrap();

        let error = plan(&fixture.home).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("could not find a Flatpak Steam library path")
        );
    }
}
