//! Durable state and pure classification for composefs -> OSTree boot cutover.
//!
//! The journal is the source of intent; firmware and ESP reads are the source
//! of truth.  A phase is never advanced from a command exit status alone.

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootTransaction {
    pub schema: u8,
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
        }
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
}
