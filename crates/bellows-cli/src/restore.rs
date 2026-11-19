//! Stage complete declared roots before replacing the previous output trees.
use anyhow::{Context, Result, bail};
use bellows_core::{Artifact, DeclaredActionRecord, atomic_write_output, validate_relative_path};
use fs2::FileExt;
use std::fs;
use std::path::{Path, PathBuf};

struct Replacement {
    destination: PathBuf,
    staging: tempfile::TempDir,
    had_old: bool,
    installed: bool,
}

pub fn declared_outputs(
    workspace: &Path,
    record: &DeclaredActionRecord,
    files: &[(&Artifact, Vec<u8>)],
) -> Result<()> {
    bellows_core::validate_output_roots(&record.inputs, &record.output_paths)?;
    // Collapse overlapping declarations to their outermost root.
    let mut roots = record
        .output_paths
        .iter()
        .map(|p| validate_relative_path(p))
        .collect::<Result<Vec<_>>>()?;
    roots.sort_by_key(|p| p.components().count());
    let mut outer = Vec::<PathBuf>::new();
    for root in roots {
        if !outer.iter().any(|parent| root.starts_with(parent)) {
            outer.push(root);
        }
    }
    let lock_dir = workspace.join(".bellows");
    fs::create_dir_all(&lock_dir)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_dir.join("restore.lock"))?;
    lock.lock_exclusive()?;
    let mut replacements = Vec::new();
    for root in outer {
        let destination = super::safe_destination(workspace, &root)?;
        let staging = tempfile::Builder::new()
            .prefix(".bellows-restore-")
            .tempdir_in(destination.parent().context("output root has no parent")?)?;
        let fresh = staging.path().join("new");
        if !files
            .iter()
            .any(|(artifact, _)| Path::new(&artifact.file_name) == root)
        {
            fs::create_dir(&fresh)?;
        }
        for (artifact, bytes) in files {
            let path = validate_relative_path(&artifact.file_name)?;
            if let Ok(relative) = path.strip_prefix(&root) {
                let target = if relative.as_os_str().is_empty() {
                    fresh.clone()
                } else {
                    fresh.join(relative)
                };
                atomic_write_output(&target, bytes)?;
                super::set_file_executable(&target, artifact.executable)?;
            }
        }
        replacements.push(Replacement {
            destination,
            staging,
            had_old: false,
            installed: false,
        });
    }
    let install = (|| -> Result<()> {
        for replacement in &mut replacements {
            if fs::symlink_metadata(&replacement.destination).is_ok() {
                fs::rename(
                    &replacement.destination,
                    replacement.staging.path().join("old"),
                )?;
                replacement.had_old = true;
            }
            fs::rename(
                replacement.staging.path().join("new"),
                &replacement.destination,
            )?;
            replacement.installed = true;
        }
        Ok(())
    })();
    if let Err(error) = install {
        let mut rollback_errors = Vec::new();
        for replacement in replacements.into_iter().rev() {
            let rollback = (|| -> Result<()> {
                if replacement.installed {
                    fs::rename(
                        &replacement.destination,
                        replacement.staging.path().join("failed"),
                    )?;
                }
                if replacement.had_old {
                    fs::rename(
                        replacement.staging.path().join("old"),
                        &replacement.destination,
                    )?;
                }
                Ok(())
            })();
            if let Err(rollback_error) = rollback {
                let saved = replacement.staging.keep();
                rollback_errors.push(format!(
                    "{rollback_error:#}; preserved backup at {}",
                    saved.display()
                ));
            }
        }
        if !rollback_errors.is_empty() {
            bail!(
                "restore failed: {error:#}; rollback: {}",
                rollback_errors.join("; ")
            );
        }
        return Err(error.context("replace declared outputs (previous outputs restored)"));
    }
    Ok(())
}
