//! What an owner may change about a version that is already published: which
//! tag points at it, and whether it may still be resolved.
//!
//! Each resolves the version before it writes, so a typo is a 404 rather than
//! a tag pointing at nothing or a yank flag set on the wrong row. Which
//! repository, which format and whether the caller may write to it are the
//! format module's gate, exactly as they are for a publish; what is ordered
//! once the gate has run is here.

use std::sync::Arc;

use crate::error::{AppError, AppResult, StoreError};
use crate::ports::packages::{NameMatch, PackageStore};

pub struct SetDistTag {
    packages: Arc<dyn PackageStore>,
}

impl SetDistTag {
    pub fn new(packages: Arc<dyn PackageStore>) -> Self {
        Self { packages }
    }

    pub async fn run(&self, package: i64, tag: &str, version: &str) -> AppResult<()> {
        let row = self
            .packages
            .version(package, version)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("version not found: {version}")))?;
        self.packages.set_dist_tag(package, tag, row.id).await?;
        Ok(())
    }
}

pub struct ClearDistTag {
    packages: Arc<dyn PackageStore>,
}

impl ClearDistTag {
    pub fn new(packages: Arc<dyn PackageStore>) -> Self {
        Self { packages }
    }

    /// Removing a tag that was never set is not an error the client can act
    /// on: the tag is gone either way.
    pub async fn run(&self, package: i64, tag: &str) -> AppResult<()> {
        match self.packages.clear_dist_tag(package, tag).await {
            Ok(()) | Err(StoreError::NotFound) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

/// cargo's yank and unyank, which are one flag and two routes.
pub struct Yank {
    packages: Arc<dyn PackageStore>,
}

impl Yank {
    pub fn new(packages: Arc<dyn PackageStore>) -> Self {
        Self { packages }
    }

    pub async fn run(
        &self,
        repository: i64,
        name: &str,
        version: &str,
        yanked: bool,
    ) -> AppResult<()> {
        let package = self
            .packages
            .package(repository, name, NameMatch::Insensitive)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("crate not found: {name}")))?;
        let row = self
            .packages
            .version(package.id, version)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("version not found: {name}@{version}")))?;
        self.packages.set_yanked(row.id, yanked).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fakes::FakeDb;

    /// A tag is refused rather than left pointing at a version that is not
    /// there, and the refusal names what was asked for.
    #[tokio::test]
    async fn a_tag_on_an_unknown_version_is_a_not_found() {
        let db = FakeDb::new();
        let refused = SetDistTag::new(db.packages())
            .run(1, "latest", "9.9.9")
            .await
            .unwrap_err();

        assert!(matches!(refused, AppError::NotFound(_)), "{refused:?}");
        assert!(refused.to_string().contains("9.9.9"), "{refused}");
    }

    /// The store's `NotFound` is the state the caller asked for, so clearing
    /// a tag nobody set succeeds.
    #[tokio::test]
    async fn clearing_a_tag_nobody_set_succeeds() {
        let db = FakeDb::new();
        ClearDistTag::new(db.packages())
            .run(1, "beta")
            .await
            .unwrap();
    }

    /// Yanking names the crate it could not find, not the version, because
    /// the package lookup is what failed.
    #[tokio::test]
    async fn yanking_an_unknown_crate_names_the_crate() {
        let db = FakeDb::new();
        let refused = Yank::new(db.packages())
            .run(1, "serde", "1.0.0", true)
            .await
            .unwrap_err();

        assert!(matches!(refused, AppError::NotFound(_)), "{refused:?}");
        assert!(refused.to_string().contains("serde"), "{refused}");
    }
}
