//! Direct (non-GitHub) artifact selection, download and verification (Go `direct.go`).

use sha2::{Digest, Sha256};

use crate::auth::REQUEST_KIND_ARTIFACT;
use crate::error::{Context, Result};
use crate::errf;
use crate::github::Client;
use crate::registry::{
    Artifact, INSTALL_TYPE_DIRECT, InstallPlan, normalize_goarch, normalize_goos, normalize_install_plan,
    validate_artifact,
};

/// Picks the artifact for the platform from a direct install plan.
pub fn select_artifact(plan: &InstallPlan, goos: &str, goarch: &str) -> Result<Artifact> {
    let plan = normalize_install_plan(plan);
    let goos = normalize_goos(goos);
    let goarch = normalize_goarch(goarch);
    if plan.kind != INSTALL_TYPE_DIRECT {
        return Err(errf!("install type {:?} is not direct", plan.kind));
    }
    plan.artifacts
        .into_iter()
        .find(|artifact| artifact.goos == goos && artifact.goarch == goarch)
        .ok_or_else(|| errf!("artifact not found for {goos}/{goarch}"))
}

impl Client {
    /// Downloads a direct artifact, capping the body at its declared size when set.
    pub async fn download_artifact(&self, ctx: &Context, artifact: &Artifact) -> Result<Vec<u8>> {
        let plan = normalize_install_plan(&InstallPlan {
            kind: INSTALL_TYPE_DIRECT.to_string(),
            artifacts: vec![artifact.clone()],
        });
        let artifact = &plan.artifacts[0];
        validate_artifact(artifact)?;
        let max_size = if artifact.size > 0 { artifact.size } else { 0 };
        let data = self
            .get(ctx, &artifact.url, "application/octet-stream", REQUEST_KIND_ARTIFACT, max_size)
            .await?;
        if max_size > 0 && data.len() as u64 > max_size as u64 {
            return Err(errf!("artifact exceeds declared size"));
        }
        Ok(data)
    }
}

/// Compares the SHA-256 of `data` with the artifact's declared checksum.
pub fn verify_artifact_checksum(artifact: &Artifact, data: &[u8]) -> Result<()> {
    let expected = artifact.sha256.trim().to_lowercase();
    if expected.is_empty() {
        return Err(errf!("artifact checksum missing"));
    }
    if hex::encode(Sha256::digest(data)) != expected {
        return Err(errf!("artifact checksum mismatch"));
    }
    Ok(())
}
