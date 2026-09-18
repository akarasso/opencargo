//! What a format decides about its names and versions. The domain holds the
//! contract only: each format's rules are built in the registry layer, and
//! the domain never parses a version.

use super::DomainError;

pub trait FormatRules: Send + Sync {
    fn validate(&self, name: &str) -> Result<(), DomainError>;

    /// The spelling every uniqueness check, store key and policy fact uses;
    /// the name as published is display data.
    fn normalize(&self, name: &str) -> String;

    /// Names a publish may never take, compared after normalization.
    fn reserved(&self) -> &'static [&'static str];

    fn validate_version(&self, version: &str) -> Result<(), DomainError>;

    /// Two spellings of one version normalize to one string.
    fn normalize_version(&self, version: &str) -> String;

    /// Validation, then the reserved-name refusal.
    fn admit(&self, name: &str) -> Result<String, DomainError> {
        self.validate(name)?;
        let normalized = self.normalize(name);
        if self.reserved().contains(&normalized.as_str()) {
            return Err(DomainError::InvalidName(format!("reserved package name: '{name}'")));
        }
        Ok(normalized)
    }
}
