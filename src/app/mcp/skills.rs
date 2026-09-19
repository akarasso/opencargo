//! `PublishSkill`: a skill archive, parsed and scanned by the format
//! adapter, placed through the shared placement and recorded in one commit.

use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::app::place::{Entry, Placer, Source};
use crate::app::publish::{repo_prefix, PublishError};
use crate::domain::layout;
use crate::ports::mcp::{McpStore, NewFinding, NewSkill};
use crate::ports::repositories::RepositoryStore;

pub struct SkillUpload {
    pub repository: i64,
    pub name: String,
    pub version: String,
    pub bytes: Bytes,
    pub description: Option<String>,
    pub allowed_tools: Option<String>,
    pub surface_sha256: String,
    pub findings: Vec<NewFinding>,
    pub blocking: i64,
    pub published_by: Option<String>,
}

pub struct PublishSkill {
    mcp: Arc<dyn McpStore>,
    repos: Arc<dyn RepositoryStore>,
    placer: Arc<Placer>,
}

impl PublishSkill {
    pub fn new(mcp: Arc<dyn McpStore>, repos: Arc<dyn RepositoryStore>, placer: Arc<Placer>) -> Self {
        Self { mcp, repos, placer }
    }

    pub async fn run(&self, upload: SkillUpload, now: DateTime<Utc>) -> Result<i64, PublishError> {
        let sha256 = format!("{:x}", Sha256::digest(&upload.bytes));
        let prefix = repo_prefix(self.repos.as_ref(), upload.repository).await?;
        let entries = [Entry {
            logical_key: layout::hosted_key(&prefix, &upload.name, &sha256, "skill.zip"),
            source: Source::Bytes(upload.bytes.clone()),
        }];
        let (upload, sha256) = (&upload, &sha256);
        let placed = self
            .placer
            .place_shared(
                &prefix,
                &entries,
                |pins| {
                    let mcp = self.mcp.clone();
                    let skill = NewSkill {
                        repository: upload.repository,
                        name: upload.name.clone(),
                        version: upload.version.clone(),
                        sha256: sha256.clone(),
                        size: upload.bytes.len() as i64,
                        description: upload.description.clone(),
                        allowed_tools: upload.allowed_tools.clone(),
                        surface_sha256: upload.surface_sha256.clone(),
                        findings: upload.findings.clone(),
                        blocking: upload.blocking,
                        published_by: upload.published_by.clone(),
                        pins,
                        now,
                    };
                    async move { mcp.publish_skill(&skill).await }
                },
                now,
            )
            .await;
        Ok(placed?)
    }
}
