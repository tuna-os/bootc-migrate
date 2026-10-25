//! Phase 1: import OSTree file objects into the composefs object store.

use super::*;
use anyhow::bail;
use composefs::fsverity::{
    FsVerityHashValue, Sha512HashValue, enable_verity_with_retry, measure_verity_opt,
};

// ---- Phase 1 ----

pub fn phase1_import_objects(report: &PreflightReport, dry_run: bool) -> Result<()> {
    println!("=== Phase 1: Importing OSTree objects ===");
    let ostree_repo = "/sysroot/ostree/repo";
    if !Path::new(ostree_repo).exists() {
        println!("No OSTree repository found. Skipping.");
        return Ok(());
    }

    let file_objects = crate::ostree::scan_ostree_file_objects(ostree_repo)
        .context("failed to scan ostree repository")?;
    let total_objects = file_objects.len();
    println!("Found {} file objects to import.", total_objects);

    if dry_run {
        println!(
            "[DRY RUN] Would import {} objects into composefs store.",
            total_objects
        );
        return Ok(());
    }

    let mut count = 0usize;
    let mut reflink_count = 0usize;
    for obj in file_objects {
        let digest = crate::composefs::compute_fsverity_sha512(&obj.path)?;
        let target_path = Path::new("/sysroot/composefs/objects").join(digest.to_object_pathname());

        if !target_path.exists() {
            let target_dir = target_path
                .parent()
                .expect("object path always has a parent directory");
            fs::create_dir_all(target_dir)?;
            if report.supports_reflink {
                if crate::reflink::reflink(&obj.path, &target_path).is_ok() {
                    reflink_count += 1;
                } else {
                    fs::copy(&obj.path, &target_path)?;
                }
            } else {
                fs::copy(&obj.path, &target_path)?;
            }
        }

        let actual = crate::composefs::compute_fsverity_sha512(&target_path)?;
        if actual != digest {
            bail!(
                "imported composefs object {} has digest {}, expected {}",
                target_path.display(),
                actual.to_hex(),
                digest.to_hex()
            );
        }
        let object = fs::File::open(&target_path)
            .with_context(|| format!("opening imported object {}", target_path.display()))?;
        match measure_verity_opt::<Sha512HashValue>(&object)
            .with_context(|| format!("measuring {}", target_path.display()))?
        {
            Some(measured) if measured != digest => bail!(
                "imported composefs object {} has fs-verity digest {}, expected {}",
                target_path.display(),
                measured.to_hex(),
                digest.to_hex()
            ),
            Some(_) => {}
            None => enable_verity_with_retry::<Sha512HashValue>(&object)
                .with_context(|| format!("enabling fs-verity on {}", target_path.display()))?,
        }
        count += 1;
        if count.is_multiple_of(1000) {
            println!("Imported {}/{} objects...", count, total_objects);
        }
    }
    println!("Imported {} objects ({} reflinked).", count, reflink_count);

    // Post-Phase-1 completeness verification: count objects in the composefs
    // store and compare with the expected total. A significant shortfall means
    // the source ostree repo had incomplete state (e.g. a pending transaction).
    let composefs_objects_dir = Path::new("/sysroot/composefs/objects");
    if composefs_objects_dir.exists() {
        let stored = crate::preflight::count_composefs_files(composefs_objects_dir);
        if stored < total_objects.saturating_sub(100) {
            // More than 100 objects short — likely a pending-transaction issue.
            eprintln!(
                "[phase1] WARNING: composefs object store has {} objects but {} were expected.\
                 The source ostree repo may have had incomplete state (e.g. an interrupted update).\
                 The resulting composefs image may not boot correctly.",
                stored, total_objects
            );
        } else {
            println!(
                "[phase1] composefs object completeness OK: {} stored, {} expected.",
                stored, total_objects
            );
        }
    }
    Ok(())
}
