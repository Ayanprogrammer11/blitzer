use super::{
    DownloadEvent,
    manifest::{Chunk, ResumeManifest, chunk_len},
};
use anyhow::{Context, Result, bail};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};
use tokio::{
    fs::{self, OpenOptions},
    io::{self as tokio_io, AsyncWriteExt},
    sync::mpsc,
};

const RESUME_MANIFEST_NAME: &str = "manifest.json";

pub(super) async fn prepare_part_dir(
    part_dir: &Path,
    expected: &ResumeManifest,
    no_resume: bool,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Result<()> {
    if no_resume && path_exists(part_dir).await? {
        fs::remove_dir_all(part_dir)
            .await
            .with_context(|| format!("failed to remove {}", part_dir.display()))?;
    }

    fs::create_dir_all(part_dir)
        .await
        .with_context(|| format!("failed to create {}", part_dir.display()))?;

    let mut reset_reason = None;
    if !no_resume {
        match read_resume_manifest(part_dir).await {
            Ok(Some(existing)) if existing.is_compatible_with(expected) => {}
            Ok(Some(_)) => {
                reset_reason = Some(
                    "Existing part files are for a different URL, size, validator, or chunk layout."
                        .to_string(),
                );
            }
            Ok(None) => {
                if part_files_present(part_dir).await? {
                    reset_reason = Some(
                        "Legacy part files have no manifest and cannot be trusted.".to_string(),
                    );
                }
            }
            Err(err) => {
                reset_reason = Some(format!("Resume manifest is unreadable ({err:#})."));
            }
        }
    }

    if let Some(reason) = reset_reason {
        let _ = tx.send(DownloadEvent::Phase(format!("{reason} Starting fresh.")));
        fs::remove_dir_all(part_dir)
            .await
            .with_context(|| format!("failed to reset {}", part_dir.display()))?;
        fs::create_dir_all(part_dir)
            .await
            .with_context(|| format!("failed to recreate {}", part_dir.display()))?;
    }

    write_resume_manifest(part_dir, expected).await
}

pub(super) async fn compute_resume_offset(part_dir: &Path, chunks: &[Chunk]) -> Result<u64> {
    let mut already_downloaded = 0u64;
    for chunk in chunks {
        let part_path = part_path_for(part_dir, chunk.index);
        let Ok(meta) = fs::metadata(&part_path).await else {
            continue;
        };

        let expected = chunk_len(*chunk);
        if meta.len() > expected {
            fs::remove_file(&part_path)
                .await
                .with_context(|| format!("failed resetting {}", part_path.display()))?;
            continue;
        }
        already_downloaded = already_downloaded.saturating_add(meta.len());
    }
    Ok(already_downloaded)
}

pub(super) async fn merge_parts(
    part_dir: &Path,
    output: &Path,
    chunks: &[Chunk],
    total_size: u64,
) -> Result<()> {
    let tmp_output = temp_output_path(output);
    remove_file_if_exists(&tmp_output).await?;

    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_output)
        .await
        .with_context(|| format!("failed to create {}", tmp_output.display()))?;

    for chunk in chunks {
        let path = part_path_for(part_dir, chunk.index);
        let expected = chunk_len(*chunk);
        let meta = fs::metadata(&path)
            .await
            .with_context(|| format!("missing part {}", path.display()))?;
        if meta.len() != expected {
            bail!(
                "part {} has size {}, expected {}",
                path.display(),
                meta.len(),
                expected
            );
        }

        let mut in_file = OpenOptions::new()
            .read(true)
            .open(&path)
            .await
            .with_context(|| format!("missing part {}", path.display()))?;
        tokio_io::copy(&mut in_file, &mut out)
            .await
            .with_context(|| format!("failed merging {}", path.display()))?;
    }
    out.flush().await.context("failed flushing merged file")?;

    let final_meta = fs::metadata(&tmp_output)
        .await
        .with_context(|| format!("failed stat {}", tmp_output.display()))?;
    if final_meta.len() != total_size {
        bail!(
            "merged file size mismatch: got {}, expected {}",
            final_meta.len(),
            total_size
        );
    }

    replace_output(&tmp_output, output).await
}

pub(super) async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

pub(super) fn part_dir_for(output: &Path) -> Result<PathBuf> {
    let file = output.file_name().context("invalid output filename")?;
    Ok(output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{}.parts", file.to_string_lossy())))
}

pub(super) fn part_path_for(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("part-{index:04}.bin"))
}

pub(super) async fn replace_output(tmp_output: &Path, output: &Path) -> Result<()> {
    match fs::rename(tmp_output, output).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(output)
                .await
                .with_context(|| format!("failed replacing {}", output.display()))?;
            fs::rename(tmp_output, output)
                .await
                .with_context(|| format!("failed moving output to {}", output.display()))
        }
        Err(e) => Err(e).with_context(|| format!("failed moving output to {}", output.display())),
    }
}

pub(super) async fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed removing {}", path.display())),
    }
}

pub(super) async fn path_exists(path: &Path) -> Result<bool> {
    match fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("failed stat {}", path.display())),
    }
}

pub(super) async fn write_resume_manifest(
    part_dir: &Path,
    manifest: &ResumeManifest,
) -> Result<()> {
    let manifest_path = manifest_path_for(part_dir);
    let tmp_path = part_dir.join(format!("{RESUME_MANIFEST_NAME}.tmp"));
    let json = serde_json::to_vec_pretty(manifest).context("failed to encode resume manifest")?;
    fs::write(&tmp_path, json)
        .await
        .with_context(|| format!("failed writing {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &manifest_path)
        .await
        .with_context(|| format!("failed replacing {}", manifest_path.display()))?;
    Ok(())
}

fn manifest_path_for(part_dir: &Path) -> PathBuf {
    part_dir.join(RESUME_MANIFEST_NAME)
}

async fn read_resume_manifest(part_dir: &Path) -> Result<Option<ResumeManifest>> {
    let manifest_path = manifest_path_for(part_dir);
    let bytes = match fs::read(&manifest_path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("failed reading {}", manifest_path.display()));
        }
    };
    let manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed parsing {}", manifest_path.display()))?;
    Ok(Some(manifest))
}

async fn part_files_present(part_dir: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(part_dir)
        .await
        .with_context(|| format!("failed reading {}", part_dir.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("failed scanning {}", part_dir.display()))?
    {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("part-") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn temp_output_path(output: &Path) -> PathBuf {
    let file = output
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "download".into());
    output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{file}.blitzer.tmp"))
}
