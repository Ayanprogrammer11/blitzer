use super::{
    super::parts::{remove_file_if_exists, replace_output},
    SegmentMeta, segment_path_for, temp_output_path,
};
use anyhow::{Context, Result, bail};
use std::{cmp::min, collections::BTreeMap, io::SeekFrom, path::Path};
use tokio::{
    fs::OpenOptions,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};

pub(super) async fn merge_overlap_segments(
    part_dir: &Path,
    output: &Path,
    metas: &[SegmentMeta],
    payload_bytes: u64,
    overlap_bytes: u64,
) -> Result<u64> {
    let tmp_output = temp_output_path(output);
    remove_file_if_exists(&tmp_output).await?;
    let by_index = metas
        .iter()
        .map(|meta| (meta.index, meta.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_output)
        .await
        .with_context(|| format!("failed to create {}", tmp_output.display()))?;

    let Some(first) = by_index.get(&0) else {
        bail!("no-range merge has no first segment");
    };
    let mut final_size = copy_segment_range(&mut out, part_dir, first.index, 0, first.len).await?;
    if first.len < payload_bytes + overlap_bytes {
        out.flush().await.context("failed flushing merged file")?;
        replace_output(&tmp_output, output).await?;
        return Ok(final_size);
    }

    let mut previous = first.clone();
    for index in 1usize.. {
        let Some(current) = by_index.get(&index).cloned() else {
            bail!("no-range merge is missing segment {index}");
        };

        let expected_overlap = read_segment_range(
            part_dir,
            previous.index,
            payload_bytes,
            min(overlap_bytes, previous.len.saturating_sub(payload_bytes)),
        )
        .await?;
        if expected_overlap.is_empty() {
            break;
        }

        let actual_overlap =
            read_segment_range(part_dir, current.index, 0, expected_overlap.len() as u64).await?;
        if actual_overlap != expected_overlap {
            bail!(
                "no-range overlap mismatch between segments {} and {}; source is not stable",
                previous.index,
                current.index
            );
        }
        if current.len < expected_overlap.len() as u64 {
            bail!(
                "no-range segment {} ended before verified overlap completed",
                current.index
            );
        }

        let append_from = expected_overlap.len() as u64;
        final_size += copy_segment_range(
            &mut out,
            part_dir,
            current.index,
            append_from,
            current.len - append_from,
        )
        .await?;

        if current.eof || current.len < payload_bytes + overlap_bytes {
            break;
        }
        previous = current;
    }

    out.flush().await.context("failed flushing merged file")?;
    replace_output(&tmp_output, output).await?;
    Ok(final_size)
}

async fn read_segment_range(
    part_dir: &Path,
    index: usize,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>> {
    let path = segment_path_for(part_dir, index);
    let mut file = OpenOptions::new()
        .read(true)
        .open(&path)
        .await
        .with_context(|| format!("failed opening {}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .await
        .with_context(|| format!("failed seeking {}", path.display()))?;
    let mut buf = vec![0; len as usize];
    let read = file
        .read(&mut buf)
        .await
        .with_context(|| format!("failed reading {}", path.display()))?;
    buf.truncate(read);
    Ok(buf)
}

async fn copy_segment_range(
    out: &mut tokio::fs::File,
    part_dir: &Path,
    index: usize,
    offset: u64,
    len: u64,
) -> Result<u64> {
    if len == 0 {
        return Ok(0);
    }

    let path = segment_path_for(part_dir, index);
    let mut file = OpenOptions::new()
        .read(true)
        .open(&path)
        .await
        .with_context(|| format!("failed opening {}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .await
        .with_context(|| format!("failed seeking {}", path.display()))?;

    let mut remaining = len;
    let mut copied = 0u64;
    let mut buf = vec![0; 64 * 1024];
    while remaining > 0 {
        let want = min(remaining, buf.len() as u64) as usize;
        let read = file
            .read(&mut buf[..want])
            .await
            .with_context(|| format!("failed reading {}", path.display()))?;
        if read == 0 {
            bail!("segment {} ended before expected copy length", index);
        }
        out.write_all(&buf[..read])
            .await
            .context("failed writing no-range merged output")?;
        remaining -= read as u64;
        copied += read as u64;
    }
    Ok(copied)
}
