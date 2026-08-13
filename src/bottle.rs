use crate::formula::{BottleFile, Formula};
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};

/// Platform tags newest-first. Bottles built for an older macOS run fine on a
/// newer one, so we take the first tag present in the formula's files map.
/// (Homebrew proper computes compatibility properly; this list is the v1 cut.)
#[cfg(target_arch = "aarch64")]
const TAG_PREFERENCE: &[&str] = &[
    "arm64_golden_gate",
    "arm64_tahoe",
    "arm64_sequoia",
    "arm64_sonoma",
    "arm64_ventura",
    "arm64_monterey",
    "arm64_big_sur",
    "all",
];

#[cfg(target_arch = "x86_64")]
const TAG_PREFERENCE: &[&str] = &[
    "golden_gate",
    "tahoe",
    "sequoia",
    "sonoma",
    "ventura",
    "monterey",
    "big_sur",
    "all",
];

pub struct Picked<'a> {
    pub tag: &'a str,
    pub file: &'a BottleFile,
}

/// Pick the best platform tag from any tag-keyed map (a formula's bottle
/// files, or a lockfile's recorded bottles).
pub fn pick_tag<T>(files: &std::collections::HashMap<String, T>) -> Option<(&'static str, &T)> {
    TAG_PREFERENCE
        .iter()
        .find_map(|tag| files.get(*tag).map(|f| (*tag, f)))
}

pub fn pick(f: &Formula) -> Result<Picked<'_>> {
    let spec = f
        .bottle
        .get("stable")
        .with_context(|| format!("{} has no stable bottle", f.name))?;
    match pick_tag(&spec.files) {
        Some((tag, file)) => Ok(Picked { tag, file }),
        None => bail!(
            "{}: no bottle for this platform (available: {})",
            f.name,
            spec.files.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Download a bottle blob from ghcr.io, streaming into `pb` and hashing
/// as we go, then verify the sha256.
///
/// Bottles are plain OCI blobs; ghcr.io allows anonymous pulls of public
/// images with the literal bearer token "QQ==" (base64 "A"), which is what
/// brew itself sends. No account, no OAuth dance.
pub async fn download(
    client: &reqwest::Client,
    file: &BottleFile,
    pb: &ProgressBar,
) -> Result<Vec<u8>> {
    let resp = client
        .get(&file.url)
        .header("Authorization", "Bearer QQ==")
        .header("Accept", "application/vnd.oci.image.layer.v1.tar+gzip")
        .send()
        .await
        .with_context(|| format!("downloading {}", file.url))?
        .error_for_status()?;

    if let Some(len) = resp.content_length() {
        pb.set_length(len);
    }

    let mut hasher = Sha256::new();
    let mut body = Vec::with_capacity(resp.content_length().unwrap_or(0) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        body.extend_from_slice(&chunk);
        pb.inc(chunk.len() as u64);
    }

    let digest = hex::encode(hasher.finalize());
    if digest != file.sha256 {
        bail!(
            "checksum mismatch for {}: expected {}, got {digest}",
            file.url,
            file.sha256
        );
    }
    Ok(body)
}
