//! Durable state and pure classification for the alongside OSTree boot
//! cutover, from a composefs host or a package-managed host.
//!
//! The journal is the source of intent; firmware, ESP and boot-filesystem
//! reads are the source of truth.  A phase is never advanced from a command
//! exit status alone.  On a package host the journal also records the
//! "Previous system" rollback staging (#462), so a cutover interrupted after
//! `bootc install` replaced `/boot` resumes that staging from the journaled
//! inputs instead of losing the way back.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const JOURNAL_FILE: &str = "boot-transaction.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootTxnPhase {
    Begun,
    EntryCreated,
    Prepared,
    Arming,
    Committed,
    RolledBack,
    RollbackUnconfirmed,
}

/// Which alongside route opened the transaction.  Defaulted so journals
/// written before package hosts were journaled still load as composefs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TxnSource {
    #[default]
    Composefs,
    Package,
}

/// Progress of the package host's "Previous system" rollback staging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RollbackStagingPhase {
    /// Not attempted yet, or interrupted before the entry was observed.
    Pending,
    /// The entry and the rollback state exist on the live system.
    Staged,
    /// The rollback state is also in the carried stateroot `/var`.
    Mirrored,
    /// Staging or mirroring failed; `error` says why.  Terminal: the run
    /// reports that `undo` is unavailable rather than retrying blind.
    Failed,
}

/// Everything needed to (re)drive the package rollback staging, journaled
/// before the boot chain is replaced so a resumed run uses the same inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRollbackStaging {
    pub phase: RollbackStagingPhase,
    pub state_dir: String,
    pub stateroot_var: String,
    pub target_boot: String,
    pub cmdline: String,
    #[serde(default)]
    pub nvram: Option<String>,
    /// Read from the target's `/boot` before any restore, then frozen.
    #[serde(default)]
    pub target_supports_bls: Option<bool>,
    /// Whether the state has to be mirrored into the carried `/var`.
    #[serde(default)]
    pub mirror: bool,
    #[serde(default)]
    pub entry_path: Option<String>,
    #[serde(default)]
    pub mirrored_to: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl PackageRollbackStaging {
    pub fn new(
        state_dir: &Path,
        stateroot_var: &str,
        target_boot: &Path,
        cmdline: &str,
        nvram: Option<&str>,
    ) -> Self {
        Self {
            phase: RollbackStagingPhase::Pending,
            state_dir: state_dir.display().to_string(),
            stateroot_var: stateroot_var.to_string(),
            target_boot: target_boot.display().to_string(),
            cmdline: cmdline.to_string(),
            nvram: nvram.map(str::to_string),
            target_supports_bls: None,
            mirror: false,
            entry_path: None,
            mirrored_to: None,
            error: None,
        }
    }

    /// Whether the journal says `bootc-rebase undo` will find its state.
    pub fn available(&self) -> bool {
        match self.phase {
            RollbackStagingPhase::Mirrored => true,
            RollbackStagingPhase::Staged => !self.mirror,
            RollbackStagingPhase::Pending | RollbackStagingPhase::Failed => false,
        }
    }
}

/// Observed state of a package transaction's boot snapshot and rollback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageObservation {
    pub snapshot_intact: bool,
    pub entry_present: bool,
    pub state_present: bool,
    pub mirror_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootTransaction {
    pub schema: u8,
    #[serde(default)]
    pub source: TxnSource,
    pub phase: BootTxnPhase,
    pub target_image: String,
    pub esp: String,
    pub snapshot_dir: String,
    pub esp_file_hashes: BTreeMap<String, String>,
    pub original_boot_order: String,
    #[serde(default)]
    pub prepared_boot_order: String,
    #[serde(default)]
    pub deployment: Option<String>,
    #[serde(default)]
    pub target_entry: Option<String>,
    #[serde(default)]
    pub rollback_entry: Option<String>,
    pub started_at_unix_secs: u64,
    pub updated_at_unix_secs: u64,
    #[serde(default)]
    pub boot_order_promoted: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// Package hosts only: the rollback staging this transaction drives.
    #[serde(default)]
    pub package_rollback: Option<PackageRollbackStaging>,
}

impl BootTransaction {
    pub fn new(
        target_image: &str,
        esp: &Path,
        snapshot_dir: &Path,
        esp_file_hashes: BTreeMap<String, String>,
        original_boot_order: String,
    ) -> Self {
        let now = now_unix_secs();
        Self {
            schema: 1,
            source: TxnSource::Composefs,
            phase: BootTxnPhase::Begun,
            target_image: target_image.to_string(),
            esp: esp.display().to_string(),
            snapshot_dir: snapshot_dir.display().to_string(),
            esp_file_hashes,
            original_boot_order,
            prepared_boot_order: String::new(),
            deployment: None,
            target_entry: None,
            rollback_entry: None,
            started_at_unix_secs: now,
            updated_at_unix_secs: now,
            boot_order_promoted: false,
            error: None,
            package_rollback: None,
        }
    }

    /// A package-host transaction: `snapshot_dir` holds the `/boot`
    /// snapshot, whose hashes are journaled so a resume can trust it.
    pub fn new_package(
        target_image: &str,
        esp: &Path,
        snapshot_dir: &Path,
        snapshot_hashes: BTreeMap<String, String>,
        original_boot_order: String,
        staging: PackageRollbackStaging,
    ) -> Self {
        let mut txn = Self::new(
            target_image,
            esp,
            snapshot_dir,
            snapshot_hashes,
            original_boot_order,
        );
        txn.source = TxnSource::Package;
        txn.package_rollback = Some(staging);
        txn
    }

    pub fn advance(&mut self, phase: BootTxnPhase) {
        self.phase = phase;
        self.updated_at_unix_secs = now_unix_secs();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootObservation {
    pub booted_ostree: bool,
    pub esp_restored: bool,
    pub target_entry_present: bool,
    pub rollback_entry_present: bool,
    pub boot_next: Option<String>,
    pub boot_order: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallClassification {
    Complete,
    Resumable,
    Failed,
    NeedsRepair,
    InProgress,
}

impl InstallClassification {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Resumable => "resumable",
            Self::Failed => "failed",
            Self::NeedsRepair => "needs-repair",
            Self::InProgress => "in-progress",
        }
    }
}

/// Classify only from durable intent plus observed boot state.
pub fn classify_install(
    txn: &BootTransaction,
    observed: &BootObservation,
) -> InstallClassification {
    let target = txn.target_entry.as_deref();
    let boot_next_is_target = target.is_some_and(|id| {
        observed
            .boot_next
            .as_deref()
            .is_some_and(|next| next.eq_ignore_ascii_case(id))
    });
    let prepared = observed.esp_restored
        && observed.target_entry_present
        && observed.rollback_entry_present
        && !txn.prepared_boot_order.is_empty()
        && observed
            .boot_order
            .eq_ignore_ascii_case(&txn.prepared_boot_order);

    match txn.phase {
        BootTxnPhase::Committed | BootTxnPhase::Arming if observed.booted_ostree => {
            InstallClassification::Complete
        }
        BootTxnPhase::Committed if boot_next_is_target && prepared => {
            InstallClassification::InProgress
        }
        BootTxnPhase::Prepared if prepared => InstallClassification::Resumable,
        BootTxnPhase::Arming if boot_next_is_target && prepared => {
            InstallClassification::InProgress
        }
        BootTxnPhase::Arming if prepared => InstallClassification::Resumable,
        BootTxnPhase::RolledBack => InstallClassification::Failed,
        BootTxnPhase::Begun
        | BootTxnPhase::EntryCreated
        | BootTxnPhase::RollbackUnconfirmed
        | BootTxnPhase::Prepared
        | BootTxnPhase::Arming
        | BootTxnPhase::Committed => InstallClassification::NeedsRepair,
    }
}

/// Classify a package-host transaction from its journal and observed state.
///
/// * `Complete`: committed, and the journaled rollback (if any) is observed.
/// * `Failed`: staging failed and was reported; nothing left to drive.
/// * `Resumable`: `Prepared` (the deployment's `/etc`, `/var` and labels are
///   done and the staging inputs are journaled) and the `/boot` snapshot
///   still matches its journaled hashes, so staging and the bootloader step
///   can be re-driven from the journal.
/// * `NeedsRepair`: interrupted before `Prepared` (the deployment may be
///   half-built), the snapshot no longer matches, or committed state has
///   disappeared.
pub fn classify_package_install(
    txn: &BootTransaction,
    observed: &PackageObservation,
) -> InstallClassification {
    let Some(staging) = txn.package_rollback.as_ref() else {
        return InstallClassification::NeedsRepair;
    };
    if txn.deployment.is_none() {
        return InstallClassification::NeedsRepair;
    }
    let staging_observed = staging.available()
        && observed.entry_present
        && observed.state_present
        && (!staging.mirror || observed.mirror_present);
    match (txn.phase, staging.phase) {
        (BootTxnPhase::Committed, RollbackStagingPhase::Failed) => InstallClassification::Failed,
        (BootTxnPhase::Committed, _) if staging_observed => InstallClassification::Complete,
        (BootTxnPhase::Committed, _) => InstallClassification::NeedsRepair,
        (BootTxnPhase::RolledBack, _) => InstallClassification::Failed,
        (BootTxnPhase::Prepared, _) if staging_observed || observed.snapshot_intact => {
            InstallClassification::Resumable
        }
        _ => InstallClassification::NeedsRepair,
    }
}

/// Drive the package rollback staging recorded in `txn`, journaling every
/// step through `persist` before the next one starts.  Already-observed
/// steps are skipped, so the same call serves the first run and a resume.
/// A step failure is recorded as [`RollbackStagingPhase::Failed`] and is not
/// an error: by now the boot chain is replaced and the caller reports that
/// `undo` is unavailable instead of aborting a bootable install.
pub fn drive_package_rollback(
    txn: &mut BootTransaction,
    exists: impl Fn(&Path) -> bool,
    stage: impl FnOnce(&PackageRollbackStaging) -> Result<PathBuf>,
    mirror: impl FnOnce(&PackageRollbackStaging) -> Result<PathBuf>,
    mut persist: impl FnMut(&BootTransaction) -> Result<()>,
) -> Result<()> {
    let staging = txn
        .package_rollback
        .as_ref()
        .context("package boot transaction has no rollback staging record")?;
    if staging.phase == RollbackStagingPhase::Failed {
        return Ok(());
    }
    let already_staged = matches!(
        staging.phase,
        RollbackStagingPhase::Staged | RollbackStagingPhase::Mirrored
    ) && staging
        .entry_path
        .as_deref()
        .is_some_and(|p| exists(Path::new(p)));
    if !already_staged {
        let result = stage(staging);
        let staging = txn.package_rollback.as_mut().expect("checked above");
        match result {
            Ok(entry) => {
                staging.phase = RollbackStagingPhase::Staged;
                staging.entry_path = Some(entry.display().to_string());
                staging.mirrored_to = None;
                staging.error = None;
            }
            Err(e) => {
                staging.phase = RollbackStagingPhase::Failed;
                staging.error = Some(format!("{e:#}"));
            }
        }
        txn.updated_at_unix_secs = now_unix_secs();
        persist(txn)?;
    }
    let staging = txn.package_rollback.as_mut().expect("checked above");
    // A journaled mirror that is no longer observed is redone, not trusted.
    if staging.phase == RollbackStagingPhase::Mirrored
        && !staging
            .mirrored_to
            .as_deref()
            .is_some_and(|p| exists(Path::new(p)))
    {
        staging.phase = RollbackStagingPhase::Staged;
    }
    let staging = txn.package_rollback.as_ref().expect("checked above");
    if staging.phase == RollbackStagingPhase::Staged && staging.mirror {
        let result = mirror(staging);
        let staging = txn.package_rollback.as_mut().expect("checked above");
        match result {
            Ok(carried) => {
                staging.phase = RollbackStagingPhase::Mirrored;
                staging.mirrored_to = Some(carried.display().to_string());
            }
            Err(e) => {
                staging.phase = RollbackStagingPhase::Failed;
                staging.error = Some(format!("{e:#}"));
            }
        }
        txn.updated_at_unix_secs = now_unix_secs();
        persist(txn)?;
    }
    Ok(())
}

/// Whether the durable snapshot alone still matches its journaled hashes.
pub fn snapshot_intact(txn: &BootTransaction) -> bool {
    !txn.esp_file_hashes.is_empty()
        && txn.esp_file_hashes.iter().all(|(rel, expected)| {
            file_sha256(&Path::new(&txn.snapshot_dir).join(rel))
                .is_ok_and(|actual| actual == *expected)
        })
}

pub fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join(JOURNAL_FILE)
}

pub fn read_journal(path: &Path) -> Result<Option<BootTransaction>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&data)
        .with_context(|| format!("parsing {}", path.display()))
        .map(Some)
}

/// Persist through a synced sibling file, rename, and parent-directory sync.
pub fn write_journal(path: &Path, txn: &BootTransaction) -> Result<()> {
    let parent = path
        .parent()
        .context("boot transaction journal has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(txn).expect("BootTransaction serializes");
    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(&data)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()
    };
    write().with_context(|| format!("writing {}", path.display()))
}

pub fn snapshot_hashes(snapshot_dir: &Path, paths: &[String]) -> Result<BTreeMap<String, String>> {
    paths
        .iter()
        .map(|rel| {
            let path = snapshot_dir.join(rel);
            file_sha256(&path).map(|hash| (rel.clone(), hash))
        })
        .collect()
}

/// Both the durable snapshot and the restored ESP copy must match the journal.
pub fn esp_files_restored(txn: &BootTransaction) -> bool {
    if txn.esp_file_hashes.is_empty() {
        return false;
    }
    let snapshot = Path::new(&txn.snapshot_dir);
    let esp = Path::new(&txn.esp);
    txn.esp_file_hashes.iter().all(|(rel, expected)| {
        [snapshot.join(rel), esp.join(rel)]
            .iter()
            .all(|path| file_sha256(path).is_ok_and(|actual| actual == *expected))
    })
}

fn file_sha256(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn txn(phase: BootTxnPhase) -> BootTransaction {
        let mut txn = BootTransaction::new(
            "quay.io/example/os:latest",
            Path::new("/efi"),
            Path::new("/var/lib/bootc-rebase/snapshot"),
            BTreeMap::from([("EFI/Linux/kernel".into(), "abc".into())]),
            "0001,0002".into(),
        );
        txn.phase = phase;
        txn.prepared_boot_order = "0002,0001".into();
        txn.target_entry = Some("0001".into());
        txn.rollback_entry = Some("0002".into());
        txn
    }

    #[test]
    fn classifier_table() {
        let prepared = BootObservation {
            esp_restored: true,
            target_entry_present: true,
            rollback_entry_present: true,
            boot_order: "0002,0001".into(),
            ..Default::default()
        };
        let mut armed = prepared.clone();
        armed.boot_next = Some("0001".into());
        let mut booted = prepared.clone();
        booted.booted_ostree = true;
        let broken = BootObservation::default();

        for (phase, observed, want) in [
            (
                BootTxnPhase::Begun,
                &broken,
                InstallClassification::NeedsRepair,
            ),
            (
                BootTxnPhase::EntryCreated,
                &broken,
                InstallClassification::NeedsRepair,
            ),
            (
                BootTxnPhase::Prepared,
                &prepared,
                InstallClassification::Resumable,
            ),
            (
                BootTxnPhase::Prepared,
                &broken,
                InstallClassification::NeedsRepair,
            ),
            (
                BootTxnPhase::Arming,
                &armed,
                InstallClassification::InProgress,
            ),
            (
                BootTxnPhase::Arming,
                &prepared,
                InstallClassification::Resumable,
            ),
            (
                BootTxnPhase::Committed,
                &armed,
                InstallClassification::InProgress,
            ),
            (
                BootTxnPhase::Committed,
                &booted,
                InstallClassification::Complete,
            ),
            (
                BootTxnPhase::Arming,
                &booted,
                InstallClassification::Complete,
            ),
            (
                BootTxnPhase::Committed,
                &broken,
                InstallClassification::NeedsRepair,
            ),
            (
                BootTxnPhase::RolledBack,
                &broken,
                InstallClassification::Failed,
            ),
            (
                BootTxnPhase::RollbackUnconfirmed,
                &broken,
                InstallClassification::NeedsRepair,
            ),
        ] {
            assert_eq!(classify_install(&txn(phase), observed), want, "{phase:?}");
        }
    }

    #[test]
    fn journal_and_esp_hashes_round_trip() {
        let d = tempdir().unwrap();
        let snapshot = d.path().join("snapshot");
        let esp = d.path().join("esp");
        fs::create_dir_all(snapshot.join("EFI/Linux")).unwrap();
        fs::create_dir_all(esp.join("EFI/Linux")).unwrap();
        fs::write(snapshot.join("EFI/Linux/kernel"), b"kernel").unwrap();
        fs::write(esp.join("EFI/Linux/kernel"), b"kernel").unwrap();
        let paths = vec!["EFI/Linux/kernel".to_string()];
        let hashes = snapshot_hashes(&snapshot, &paths).unwrap();
        let txn = BootTransaction::new("image", &esp, &snapshot, hashes, "0001".into());
        assert!(esp_files_restored(&txn));

        let path = journal_path(d.path());
        write_journal(&path, &txn).unwrap();
        assert_eq!(read_journal(&path).unwrap(), Some(txn.clone()));
        assert!(!path.with_extension("json.tmp").exists());

        fs::write(esp.join("EFI/Linux/kernel"), b"changed").unwrap();
        assert!(!esp_files_restored(&txn));
    }

    fn package_txn(
        phase: BootTxnPhase,
        staging_phase: RollbackStagingPhase,
        mirror: bool,
    ) -> BootTransaction {
        let mut staging = PackageRollbackStaging::new(
            Path::new("/var/lib/bootc-rebase/package-rollback"),
            "/ostree/deploy/default/var",
            Path::new("/boot"),
            "root=UUID=abc ro",
            None,
        );
        staging.phase = staging_phase;
        staging.mirror = mirror;
        let mut txn = BootTransaction::new_package(
            "quay.io/example/os:latest",
            Path::new("/boot/efi"),
            Path::new("/var/lib/bootc-rebase/boot-snapshot-1"),
            BTreeMap::from([("vmlinuz-6.1".into(), "abc".into())]),
            "0001".into(),
            staging,
        );
        txn.phase = phase;
        txn.deployment = Some("abc.0".into());
        txn
    }

    #[test]
    fn package_classifier_table() {
        use BootTxnPhase as P;
        use InstallClassification as C;
        use RollbackStagingPhase as R;
        let intact = PackageObservation {
            snapshot_intact: true,
            ..Default::default()
        };
        let staged = PackageObservation {
            snapshot_intact: true,
            entry_present: true,
            state_present: true,
            mirror_present: false,
        };
        let mirrored = PackageObservation {
            mirror_present: true,
            ..staged.clone()
        };
        let lost = PackageObservation::default();

        for (phase, staging, mirror, observed, want) in [
            // Interrupted before /etc, /var and labels were done: never resumed.
            (P::Begun, R::Pending, false, &intact, C::NeedsRepair),
            (P::EntryCreated, R::Pending, true, &intact, C::NeedsRepair),
            // Prepared: staging inputs journaled; re-drive from the snapshot.
            (P::Prepared, R::Pending, true, &intact, C::Resumable),
            (P::Prepared, R::Staged, true, &staged, C::Resumable),
            (P::Prepared, R::Mirrored, true, &mirrored, C::Resumable),
            (P::Prepared, R::Failed, true, &intact, C::Resumable),
            (P::Prepared, R::Pending, true, &lost, C::NeedsRepair),
            // Committed: the journaled rollback must still be observed.
            (P::Committed, R::Mirrored, true, &mirrored, C::Complete),
            (P::Committed, R::Staged, false, &staged, C::Complete),
            (P::Committed, R::Staged, true, &staged, C::NeedsRepair),
            (P::Committed, R::Mirrored, true, &lost, C::NeedsRepair),
            (P::Committed, R::Failed, true, &lost, C::Failed),
        ] {
            let txn = package_txn(phase, staging, mirror);
            assert_eq!(
                classify_package_install(&txn, observed),
                want,
                "{phase:?}/{staging:?}/mirror={mirror}"
            );
        }

        let mut no_deployment = package_txn(P::Prepared, R::Pending, true);
        no_deployment.deployment = None;
        assert_eq!(
            classify_package_install(&no_deployment, &intact),
            C::NeedsRepair
        );
        let mut no_staging = package_txn(P::Prepared, R::Pending, true);
        no_staging.package_rollback = None;
        assert_eq!(
            classify_package_install(&no_staging, &intact),
            C::NeedsRepair
        );
    }

    fn staging_phase(txn: &BootTransaction) -> RollbackStagingPhase {
        txn.package_rollback.as_ref().unwrap().phase
    }

    #[test]
    fn drive_journals_each_rollback_step() {
        let mut txn = package_txn(BootTxnPhase::Prepared, RollbackStagingPhase::Pending, true);
        let mut journaled = Vec::new();
        drive_package_rollback(
            &mut txn,
            |_| true,
            |_| Ok(PathBuf::from("/boot/loader/entries/previous.conf")),
            |_| {
                Ok(PathBuf::from(
                    "/ostree/deploy/default/var/lib/bootc-rebase/package-rollback",
                ))
            },
            |t| {
                journaled.push(staging_phase(t));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            journaled,
            [RollbackStagingPhase::Staged, RollbackStagingPhase::Mirrored]
        );
        let staging = txn.package_rollback.as_ref().unwrap();
        assert!(staging.available());
        assert_eq!(
            staging.entry_path.as_deref(),
            Some("/boot/loader/entries/previous.conf")
        );
    }

    #[test]
    fn drive_without_mirror_stops_at_staged() {
        let mut txn = package_txn(BootTxnPhase::Prepared, RollbackStagingPhase::Pending, false);
        drive_package_rollback(
            &mut txn,
            |_| true,
            |_| Ok(PathBuf::from("/boot/previous.conf")),
            |_| panic!("no carried /var, nothing to mirror"),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(staging_phase(&txn), RollbackStagingPhase::Staged);
        assert!(txn.package_rollback.as_ref().unwrap().available());
    }

    #[test]
    fn drive_records_failures_without_erroring() {
        let mut txn = package_txn(BootTxnPhase::Prepared, RollbackStagingPhase::Pending, true);
        let mut persisted = 0;
        drive_package_rollback(
            &mut txn,
            |_| true,
            |_| anyhow::bail!("no initramfs for vmlinuz-6.1"),
            |_| panic!("mirror must not run after a failed stage"),
            |_| {
                persisted += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(persisted, 1);
        let staging = txn.package_rollback.as_ref().unwrap();
        assert_eq!(staging.phase, RollbackStagingPhase::Failed);
        assert!(!staging.available());
        assert!(staging.error.as_deref().unwrap().contains("no initramfs"));

        let mut txn = package_txn(BootTxnPhase::Prepared, RollbackStagingPhase::Pending, true);
        drive_package_rollback(
            &mut txn,
            |_| true,
            |_| Ok(PathBuf::from("/boot/previous.conf")),
            |_| anyhow::bail!("ENOSPC"),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(staging_phase(&txn), RollbackStagingPhase::Failed);

        // Failed is terminal: a resume reports it rather than retrying.
        drive_package_rollback(
            &mut txn,
            |_| true,
            |_| panic!("failed staging is not retried"),
            |_| panic!("failed staging is not retried"),
            |_| panic!("nothing to journal"),
        )
        .unwrap();
    }

    #[test]
    fn resume_skips_observed_steps_and_redoes_lost_ones() {
        // Interrupted after staging, before the mirror: only the mirror runs.
        let mut txn = package_txn(BootTxnPhase::Prepared, RollbackStagingPhase::Staged, true);
        txn.package_rollback.as_mut().unwrap().entry_path = Some("/boot/previous.conf".into());
        drive_package_rollback(
            &mut txn,
            |_| true,
            |_| panic!("an observed entry is not re-staged"),
            |_| Ok(PathBuf::from("/carried")),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(staging_phase(&txn), RollbackStagingPhase::Mirrored);

        // Journaled as mirrored, but the carried copy is gone: mirror again.
        let mut mirrored_again = false;
        drive_package_rollback(
            &mut txn,
            |p| p != Path::new("/carried"),
            |_| panic!("an observed entry is not re-staged"),
            |_| {
                mirrored_again = true;
                Ok(PathBuf::from("/carried"))
            },
            |_| Ok(()),
        )
        .unwrap();
        assert!(mirrored_again);

        // Journaled as staged, but the entry is gone: stage again.
        let mut txn = package_txn(BootTxnPhase::Prepared, RollbackStagingPhase::Staged, false);
        txn.package_rollback.as_mut().unwrap().entry_path = Some("/boot/previous.conf".into());
        let mut staged_again = false;
        drive_package_rollback(
            &mut txn,
            |_| false,
            |_| {
                staged_again = true;
                Ok(PathBuf::from("/boot/previous.conf"))
            },
            |_| panic!("no mirror requested"),
            |_| Ok(()),
        )
        .unwrap();
        assert!(staged_again);
    }

    #[test]
    fn package_journal_round_trips_and_old_journals_load_as_composefs() {
        let d = tempdir().unwrap();
        let snapshot = d.path().join("boot-snapshot");
        fs::create_dir_all(&snapshot).unwrap();
        fs::write(snapshot.join("vmlinuz-6.1"), b"kernel").unwrap();
        let hashes = snapshot_hashes(&snapshot, &["vmlinuz-6.1".to_string()]).unwrap();
        let mut pkg = package_txn(BootTxnPhase::Prepared, RollbackStagingPhase::Staged, true);
        pkg.snapshot_dir = snapshot.display().to_string();
        pkg.esp_file_hashes = hashes;
        assert!(snapshot_intact(&pkg));

        let path = journal_path(d.path());
        write_journal(&path, &pkg).unwrap();
        let loaded = read_journal(&path).unwrap().unwrap();
        assert_eq!(loaded, pkg);
        assert_eq!(loaded.source, TxnSource::Package);

        fs::write(snapshot.join("vmlinuz-6.1"), b"overwritten").unwrap();
        assert!(!snapshot_intact(&pkg));

        let mut legacy = serde_json::to_value(txn(BootTxnPhase::Prepared)).unwrap();
        let obj = legacy.as_object_mut().unwrap();
        obj.remove("source");
        obj.remove("package_rollback");
        let legacy: BootTransaction = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.source, TxnSource::Composefs);
        assert!(legacy.package_rollback.is_none());
    }
}
