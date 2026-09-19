//! What a format decides about its names and versions. The domain holds the
//! contract only: each format's rules are built in the registry layer, and
//! the domain never parses a version.

use super::routing::Pattern;
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

    /// Store identity on the **read** path: two spellings share an
    /// `ident_key` exactly when the store serves them as one row. Routing
    /// exceptions compare on this and on nothing coarser, so an exception
    /// never reopens a spelling the administrator did not write.
    fn ident_key(&self, name: &str) -> String;

    /// The pattern-matching key: a coarsening of [`Self::ident_key`], with
    /// `ker(ident_key) ⊆ ker(match_key)` over every spelling the format's
    /// read validator admits. Coarsening on a pattern refuses more, which is
    /// conservative; that is the only side it is allowed on.
    fn match_key(&self, name: &str) -> String;

    /// A routing pattern, canonicalized by the same function as
    /// [`Self::match_key`] — the asymmetry between the two is what a spelling
    /// would escape through.
    fn canonical_pattern(&self, pattern: &str) -> Result<Pattern, DomainError>;

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
