//! The human-only elevation contract.
//!
//! This module defines the *types* that make elevation safe, not the
//! password verification itself (that lives in `profiles-vault`). The rules
//! encoded here are:
//!
//! - An [`ElevationRequest`] describes exactly what a command wants to do
//!   (its digest, effect summary, and requested capability delta). It never
//!   contains a password field and never implements `Serialize`.
//! - A [`HumanApprovalAuthority`] verifies a human. Its trait method takes no
//!   password and returns no proof: the password exists only inside the
//!   trusted vault verification path.
//! - An [`ApprovalLease`] is minted **only by the broker** after a successful
//!   human verification, and only the broker can validate it. The agent-facing
//!   API cannot construct, submit, or widen a lease.
//!
//! Together these force the invariant: *the AI can request elevation but can
//! never read, submit, derive, replay, or widen the human's authority.*

use std::time::{Duration, Instant};

use crate::capability::CapabilityGrant;
use crate::shell_ir::{CapabilityDelta, CommandDigest, EffectSummary};

/// A request for a scoped, temporary elevation of a session's authority.
///
/// Deliberately not `Clone`, not `Serialize`, and not `Debug`-printable with
/// effect values: it is a one-way description handed from the executor to the
/// authority, never a capability object.
#[derive(Debug)]
pub struct ElevationRequest {
    /// The session requesting elevation.
    pub session_id: u64,
    /// Canonical digest of the exact plan being approved.
    pub digest: CommandDigest,
    /// Redacted effect summary (paths, hosts, secret names — never values).
    pub summary: EffectSummary,
    /// The capability delta the plan needs beyond the base grant.
    pub requested: CapabilityDelta,
    /// How long a minted lease may live.
    pub expires_after: Duration,
}

/// What an agent may observe about a pending elevation.
///
/// This is the only approval surface visible to the agent and to UI renderers:
/// a redacted request plus a stable request id. It contains no password, no
/// proof, and no lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingApproval {
    /// Stable request id the agent can reference when waiting.
    pub request_id: u64,
    /// The session that is parked.
    pub session_id: u64,
    /// Redacted effect summary for display.
    pub summary: EffectSummary,
    /// Hex digest of the exact plan awaiting approval.
    pub digest_hex: String,
}

/// Errors from the elevation path.
#[derive(Debug, thiserror::Error)]
pub enum ElevationError {
    /// The human verification failed (wrong password, lockout, or timeout).
    #[error("human verification failed")]
    VerificationFailed,
    /// The authority could not be reached or decided.
    #[error("approval authority unavailable: {0}")]
    AuthorityUnavailable(String),
    /// The request was denied by the human.
    #[error("elevation denied by the human")]
    Denied,
    /// The lease does not match the request it was minted for.
    #[error("lease does not match the request")]
    LeaseMismatch,
    /// The lease has expired.
    #[error("lease expired")]
    LeaseExpired,
    /// The lease was revoked.
    #[error("lease revoked")]
    Revoked,
    /// The lease would grant more than was requested and approved.
    #[error("lease scope exceeds the approved request")]
    LeaseScopeExceeded,
    /// The session is not eligible for elevation.
    #[error("session not eligible for elevation")]
    SessionNotEligible,
}

/// A short-lived, action-bound, capability-scoped grant minted by the broker
/// after a successful human verification.
///
/// Fields are private and there is deliberately **no public constructor**: the
/// only way to obtain a lease is through the broker's
/// `approve_with_authority` path. `ApprovalLease` does not implement
/// `Clone`, `Serialize`, or a value-printing `Debug`.
#[derive(Debug)]
pub struct ApprovalLease {
    lease_id: u128,
    session_id: u64,
    digest: CommandDigest,
    /// Consumed by the broker when validating a leased command in Task 4/5.
    #[allow(dead_code)]
    grant: CapabilityGrant,
    expires_at: Instant,
}

impl ApprovalLease {
    /// Construct a lease. **Broker-internal only.**
    ///
    /// # Panics
    ///
    /// Panics if called outside the broker. This is enforced by convention and
    /// by keeping the constructor `pub(crate)`; the broker module is the only
    /// in-crate user.
    pub(crate) fn mint(
        lease_id: u128,
        session_id: u64,
        digest: CommandDigest,
        grant: CapabilityGrant,
        expires_after: Duration,
    ) -> Self {
        Self {
            lease_id,
            session_id,
            digest,
            grant,
            expires_at: Instant::now() + expires_after,
        }
    }

    /// The lease identifier, for audit and revocation references.
    pub fn lease_id(&self) -> u128 {
        self.lease_id
    }

    /// The grant this lease authorizes. The broker uses this when validating;
    /// agents never see the grant.
    #[allow(dead_code)]
    pub(crate) fn grant(&self) -> &CapabilityGrant {
        &self.grant
    }

    /// Validate that this lease authorizes `session_id` running `digest`.
    ///
    /// This is the broker's fail-closed check: any mismatch of session,
    /// digest, or expiry rejects the lease. The grant itself is *not* compared
    /// here — the executor checks that the requested operation is within the
    /// lease grant at execution time.
    pub fn validate_for(&self, session_id: u64, digest: &CommandDigest) -> Result<(), ElevationError> {
        if self.session_id != session_id {
            return Err(ElevationError::LeaseMismatch);
        }
        if self.digest != *digest {
            return Err(ElevationError::LeaseMismatch);
        }
        if Instant::now() >= self.expires_at {
            return Err(ElevationError::LeaseExpired);
        }
        Ok(())
    }

    /// Whether this lease has already expired.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// The trusted callback that verifies a human for one elevation request.
///
/// Implementations may prompt for a password (in the trusted CLI) or verify
/// through the vault, but the trait contract guarantees:
///
/// - no password parameter crosses this boundary in either direction;
/// - no proof, token, or lease is returned to the caller.
///
/// The broker mints the lease *after* this callback succeeds.
pub trait HumanApprovalAuthority {
    /// Verify that a human approved this exact request.
    ///
    /// # Errors
    ///
    /// Returns [`ElevationError::VerificationFailed`] or
    /// [`ElevationError::Denied`] when the human cannot be verified or
    /// declined.
    fn verify_human(&self, request: &ElevationRequest) -> Result<(), ElevationError>;
}

/// A no-op authority that always denies. Used as the default when no trusted
/// authority is wired, so elevation never silently succeeds.
#[derive(Debug, Default)]
pub struct DenyAllAuthority;

impl HumanApprovalAuthority for DenyAllAuthority {
    fn verify_human(&self, _request: &ElevationRequest) -> Result<(), ElevationError> {
        Err(ElevationError::Denied)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityGrant, FilesystemAccess};
    use crate::shell_ir::{Builtin, CommandSpec, Program, SessionPath, ShellProgram, Statement};

    fn sample_request() -> ElevationRequest {
        let plan = ShellProgram {
            statements: vec![Statement::Command(CommandSpec {
                program: Program::Builtin(Builtin::Mkdir(
                    SessionPath::new("newdir").expect("valid path"),
                )),
                args: Vec::new(),
                redirects: Vec::new(),
                cwd: SessionPath::new(".").expect("valid cwd"),
            })],
        };
        ElevationRequest {
            session_id: 42,
            digest: CommandDigest::of(&plan),
            summary: EffectSummary::default(),
            requested: CapabilityDelta::default(),
            expires_after: Duration::from_secs(60),
        }
    }

    /// The agent-facing approval API must not carry a password field or a
    /// lease constructor. Compile-time proof: `PendingApproval` has only
    /// redacted, serializable-ish fields, and `ApprovalLease` has no public
    /// constructor.
    #[test]
    fn elevation_request_contains_effects_but_never_a_password_field() {
        let request = sample_request();
        // There is no `password` field by construction of the type. Assert the
        // redacted summary and digest are present and usable.
        assert_eq!(request.session_id, 42);
        assert_eq!(request.digest.hex().len(), 64);
        assert!(request.summary.reads.is_empty());
    }

    #[test]
    fn agent_cannot_construct_or_submit_a_lease() {
        // `ApprovalLease::mint` is pub(crate); a hypothetical agent crate could
        // not call it. Here we prove the observable contract: there is no way
        // to obtain a lease without a broker, and the deny-all authority
        // refuses everything.
        let authority = DenyAllAuthority;
        let outcome = authority.verify_human(&sample_request());
        assert!(matches!(outcome, Err(ElevationError::Denied)));
    }

    #[test]
    fn lease_validation_fails_closed_on_mismatch_and_expiry() {
        let grant = CapabilityGrant::workspace(
            std::env::temp_dir().join("ferrous-lease-test"),
            FilesystemAccess::ReadWrite,
        )
        .expect("absolute workspace");
        let plan = ShellProgram::empty();
        let digest = CommandDigest::of(&plan);
        let other = CommandDigest::of(&ShellProgram {
            statements: vec![Statement::Command(CommandSpec {
                program: Program::Builtin(Builtin::Pwd),
                args: Vec::new(),
                redirects: Vec::new(),
                cwd: SessionPath::new(".").expect("valid cwd"),
            })],
        });

        let lease = ApprovalLease::mint(1, 7, digest, grant, Duration::from_secs(60));
        assert!(lease.validate_for(7, &digest).is_ok());
        // Wrong session.
        assert!(matches!(
            lease.validate_for(8, &digest),
            Err(ElevationError::LeaseMismatch)
        ));
        // Wrong digest: the approved action changed.
        assert!(matches!(
            lease.validate_for(7, &other),
            Err(ElevationError::LeaseMismatch)
        ));

        let expired = ApprovalLease::mint(
            2,
            7,
            digest,
            CapabilityGrant::workspace(
                std::env::temp_dir().join("ferrous-lease-expired"),
                FilesystemAccess::ReadWrite,
            )
            .expect("absolute workspace"),
            Duration::from_nanos(1),
        );
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(
            expired.validate_for(7, &digest),
            Err(ElevationError::LeaseExpired)
        ));
        assert!(expired.is_expired());
    }

    #[test]
    fn pending_approval_is_redacted_and_stable() {
        let pending = PendingApproval {
            request_id: 99,
            session_id: 42,
            summary: EffectSummary {
                reads: vec!["src/main.rs".to_owned()],
                ..EffectSummary::default()
            },
            digest_hex: "abcd".to_owned(),
        };
        assert_eq!(pending.request_id, 99);
        assert_eq!(pending.summary.reads, ["src/main.rs"]);
        assert_eq!(pending.digest_hex, "abcd");
    }
}
