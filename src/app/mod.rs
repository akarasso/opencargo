//! The use cases: one per protocol operation and admin action.
//!
//! A use case holds the ports it needs and nothing else — never the
//! application state, never a handler's request. It owns the order of the
//! calls, what is atomic and what is recorded; the driving adapter above it
//! parses, calls one of these, and encodes the answer.
//!
//! The rest of this note is the review §1.2 asks for at the close of the
//! boundary work: every use case that section names is either here, or struck
//! with the reason, so that nothing is left standing as an aspiration.
//!
//! Here. The three publishes are one `publish::PublishVersion`, because npm,
//! cargo and go differ in what they parse and not at all in the order it
//! lands; `publish_tail` is the gate and the tail they run either side of it.
//! `PutOciManifest` and `CompleteUpload` are `oci`; `PromoteVersion` is
//! `promote`; `SetDistTag` and `Yank` are `releases`, with the clear of a tag
//! beside them; `CreateRepository`, `UpdateRepository` and `DeleteRepository`
//! are `repositories`, over the rules in `repo_spec`; `CreateUser`,
//! `UpdateUser` and `DeleteUser` are `users`, with the self-service password
//! change beside them; `IssueToken` and `RevokeToken` are `tokens`;
//! `SetPermission` is `permissions`, with its withdrawal; `CreateWebhook` is
//! `webhooks`; `ScanVersion` is `scan`; `Authenticate` is `authenticate`,
//! behind the auth middleware, npm login, the token endpoint and the
//! password change; `search` is the one read that is a use case rather than a
//! leaf, because merging the hosted index with what the proxy served is a
//! rule and not a rendering. `events` and `audit` are not use
//! cases but the two tails every one of them ends with: which audience an
//! event has, and what the trail records.
//!
//! Struck, each with what it is instead:
//! - every protocol *read* — `ResolvePackument`, `ResolveTarball`,
//!   `ResolveDistTags`, `ResolveCargoIndex`, `ResolveCrate`, `ResolveGo*`,
//!   `ResolveOci*`, `Search` — is a `Leaf` over `Cx`, which is already a
//!   struct holding nothing but ports (`registry/resolve.rs`);
//! - `StartOciUpload` and `AppendChunk`: one port call and one storage call
//!   behind their own authorization, ordering nothing;
//! - `RunCleanup`: the same shape over four ports in `telemetry/cleanup.rs`,
//!   driven by a background task rather than by a handler;
//! - `ReportTotals`: one delegation to the memoized snapshot the policy
//!   engine holds over `PolicyStore` (`policy/mod.rs`), which is parse, call
//!   one port, encode;
//! - `AuthorizeRepoAction`: a pure domain function over one grant lookup
//!   (`domain::allows`), applied by the format modules as their write gate.

pub mod audit;
pub mod authenticate;
pub mod events;
pub mod import;
pub mod maven;
pub mod mark;
pub mod mcp;
pub mod nuget;
pub mod lease;
pub mod login_gate;
pub mod oci;
pub mod permissions;
pub mod place;
pub mod promote;
pub mod pypi;
pub mod publish;
pub mod publish_tail;
pub mod reclaim;
pub mod reconcile;
pub mod releases;
pub mod repo_spec;
pub mod repositories;
pub mod scan;
pub mod search;
pub mod storage_ops;
pub mod sso;
pub mod sweep_storage;
pub mod tokens;
pub mod users;
pub mod webhooks;
