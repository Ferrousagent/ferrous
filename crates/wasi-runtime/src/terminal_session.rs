//! Persistent terminal session state for the Ferrous Shell.
//!
//! A [`TerminalSession`] is the durable state a human or AI interacts with
//! across many commands: a capability-scoped working directory, an approved
//! environment overlay, and a bounded table of session-owned jobs. `cd`
//! changes only session state — never the host process cwd — and every
//! resolved path is re-checked against the session's capability grant.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::capability::{CapabilityGrant, ResourceLimits};
use crate::command::Actor;
use crate::elevation::ApprovalLease;
use crate::shell_ir::SessionPath;

/// Identifies one persistent terminal session.
pub type SessionId = u64;

/// How many live jobs one session may own before new ones are rejected.
pub const DEFAULT_MAX_JOBS: usize = 16;

/// The immutable specification of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalSessionSpec {
    /// Stable session identifier.
    pub id: SessionId,
    /// The principal using this session.
    pub actor: Actor,
    /// Capability-relative initial working directory (usually `.`).
    pub cwd: SessionPath,
    /// Base authority for the session.
    pub base_grant: CapabilityGrant,
    /// Resource limits applied to every job in the session.
    pub limits: ResourceLimits,
}

/// A delta applied to the session's environment overlay.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvDelta {
    /// Variables to set in the overlay.
    pub set: BTreeMap<String, String>,
    /// Variable names to remove from the overlay.
    pub remove: Vec<String>,
}

/// A session-owned background job handle.
#[derive(Debug)]
pub struct JobHandle {
    /// The command's plan digest hex, for audit.
    pub digest_hex: String,
    /// Cancellation for this job.
    pub cancel: crate::cancel::CancelHandle,
}

/// Errors produced by session operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The requested path is outside the session's capability grant.
    #[error("path denied by capability policy: {0}")]
    PathDenied(PathBuf),
    /// The session has been closed.
    #[error("session is closed")]
    Closed,
    /// The job table is full.
    #[error("session job table is full (max {0} jobs)")]
    JobTableFull(usize),
    /// The job does not exist.
    #[error("no job with id {0}")]
    UnknownJob(u64),
    /// The environment variable name was rejected by the capability policy.
    #[error("environment variable denied by capability policy: {0}")]
    EnvironmentDenied(String),
    /// The path does not exist on disk.
    #[error("path does not exist: {0}")]
    NotFound(PathBuf),
    /// The operation failed on the filesystem.
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

/// The persistent state of one terminal session.
#[derive(Debug)]
pub struct TerminalSession {
    spec: TerminalSessionSpec,
    /// Absolute path of the session's current working directory, always
    /// inside the base grant's root.
    cwd: PathBuf,
    /// Session environment overlay. Only allowlisted names may enter it.
    env: BTreeMap<String, String>,
    /// Live session-owned jobs, keyed by job id.
    jobs: HashMap<u64, JobHandle>,
    /// The next job id to assign.
    next_job_id: u64,
    /// An optional active elevation lease for the session.
    lease: Option<ApprovalLease>,
    /// Whether the session has been closed.
    closed: bool,
}

impl TerminalSession {
    /// Create a session, resolving the initial cwd against the base grant.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::PathDenied`] if the initial cwd is outside the
    /// grant, or [`SessionError::NotFound`] if it does not exist.
    pub fn new(spec: TerminalSessionSpec) -> Result<Self, SessionError> {
        let root = workspace_root(&spec.base_grant)?;
        let cwd = resolve_session_path(&root, spec.cwd.as_str())?;
        if !spec.base_grant.allows_existing_path(&cwd) {
            return Err(SessionError::PathDenied(cwd));
        }
        Ok(Self {
            spec,
            cwd,
            env: BTreeMap::new(),
            jobs: HashMap::new(),
            next_job_id: 0,
            lease: None,
            closed: false,
        })
    }

    /// The session specification.
    pub fn spec(&self) -> &TerminalSessionSpec {
        &self.spec
    }

    /// The session id.
    pub fn id(&self) -> SessionId {
        self.spec.id
    }

    /// The actor using the session.
    pub fn actor(&self) -> Actor {
        self.spec.actor
    }

    /// The base capability grant.
    pub fn base_grant(&self) -> &CapabilityGrant {
        &self.spec.base_grant
    }

    /// The absolute current working directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The current working directory as a display string relative to the
    /// workspace root when possible, else the absolute path.
    pub fn cwd_display(&self) -> String {
        let root = workspace_root(&self.spec.base_grant).unwrap_or_else(|_| self.cwd.clone());
        match self.cwd.strip_prefix(&root) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => self.cwd.to_string_lossy().into_owned(),
        }
    }

    /// The environment overlay.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// The session's resource limits.
    pub fn limits(&self) -> ResourceLimits {
        self.spec.limits
    }

    /// The active elevation lease, if any.
    pub fn lease(&self) -> Option<&ApprovalLease> {
        self.lease.as_ref()
    }

    /// Attach a broker-minted lease to the session.
    ///
    /// This is the *only* way a lease enters the session, and only the broker
    /// can mint one. A stale lease replaces the previous one.
    pub fn attach_lease(&mut self, lease: ApprovalLease) {
        self.lease = Some(lease);
    }

    /// Clear the active lease (after completion, revocation, or expiry).
    pub fn clear_lease(&mut self) {
        self.lease = None;
    }

    /// Resolve a capability-relative path against the current cwd and verify
    /// it stays inside the base grant (symlink-aware).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::PathDenied`] on any escape.
    pub fn resolve_path(&self, path: &SessionPath) -> Result<PathBuf, SessionError> {
        let resolved = resolve_session_path(&self.cwd, path.as_str())?;
        if !self.spec.base_grant.allows_existing_path(&resolved) {
            return Err(SessionError::PathDenied(resolved));
        }
        Ok(resolved)
    }

    /// Resolve a path that is allowed to *not exist yet* (e.g. mkdir target).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::PathDenied`] on any escape.
    pub fn resolve_new_path(&self, path: &SessionPath) -> Result<PathBuf, SessionError> {
        let resolved = resolve_session_path(&self.cwd, path.as_str())?;
        if !self.spec.base_grant.allows_path(&resolved) {
            return Err(SessionError::PathDenied(resolved));
        }
        Ok(resolved)
    }

    /// Change the session working directory.
    ///
    /// `cd` only mutates session state; the host process cwd is never touched.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::PathDenied`] if the target escapes the grant,
    /// or [`SessionError::NotFound`] if it is not a directory.
    pub fn change_dir(&mut self, path: &SessionPath) -> Result<(), SessionError> {
        let target = self.resolve_path(path)?;
        if !target.is_dir() {
            return Err(SessionError::NotFound(target));
        }
        self.cwd = target;
        Ok(())
    }

    /// Apply an environment delta to the overlay, enforcing the allowlist.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::EnvironmentDenied`] if any name is not
    /// allowlisted in the base grant.
    pub fn apply_env(&mut self, delta: EnvDelta) -> Result<(), SessionError> {
        for name in delta.set.keys().chain(delta.remove.iter()) {
            if !self.spec.base_grant.allows_environment(name) {
                return Err(SessionError::EnvironmentDenied(name.clone()));
            }
        }
        for name in delta.remove {
            self.env.remove(&name);
        }
        self.env.extend(delta.set);
        Ok(())
    }

    /// Register a session-owned job and return its id.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::JobTableFull`] when the table is at capacity,
    /// or [`SessionError::Closed`] when the session is closed.
    pub fn register_job(&mut self, digest_hex: String, cancel: crate::cancel::CancelHandle) -> Result<u64, SessionError> {
        if self.closed {
            return Err(SessionError::Closed);
        }
        if self.jobs.len() >= DEFAULT_MAX_JOBS {
            return Err(SessionError::JobTableFull(DEFAULT_MAX_JOBS));
        }
        let id = self.next_job_id;
        self.next_job_id = self.next_job_id.saturating_add(1);
        self.jobs.insert(id, JobHandle { digest_hex, cancel });
        Ok(id)
    }

    /// Cancel one session-owned job and remove it from the table.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownJob`] if the id is not live.
    pub fn cancel_job(&mut self, id: u64) -> Result<(), SessionError> {
        let handle = self
            .jobs
            .remove(&id)
            .ok_or(SessionError::UnknownJob(id))?;
        handle.cancel.cancel();
        Ok(())
    }

    /// Number of live jobs.
    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }

    /// Close the session: cancel every owned job, release the lease, and mark
    /// the session closed so no further jobs can be registered.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        for handle in self.jobs.values() {
            handle.cancel.cancel();
        }
        self.jobs.clear();
        self.lease = None;
    }

    /// Whether the session is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

/// The absolute workspace root of a grant: the root of its first filesystem
/// grant, which `TerminalSession::new` resolves the initial cwd against.
fn workspace_root(grant: &CapabilityGrant) -> Result<PathBuf, SessionError> {
    let roots: Vec<PathBuf> = grant
        .filesystem_grants()
        .map(|filesystem| filesystem.root().to_path_buf())
        .collect();
    roots
        .into_iter()
        .next()
        .ok_or_else(|| SessionError::PathDenied(PathBuf::from("<no filesystem grant>")))
}

/// Join a session path onto a base, collapsing `.` and `..` lexically.
fn resolve_session_path(base: &Path, session_path: &str) -> Result<PathBuf, SessionError> {
    let mut resolved = base.to_path_buf();
    for component in session_path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                if !resolved.pop() {
                    return Err(SessionError::PathDenied(resolved));
                }
            }
            _ => resolved.push(component),
        }
    }
    Ok(resolved)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityGrant, FilesystemAccess};

    fn test_spec(actor: Actor) -> (TerminalSessionSpec, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "ferrous-session-test-{}-{actor:?}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("test root is created");
        std::fs::create_dir_all(root.join("sub")).expect("subdir is created");
        let grant = CapabilityGrant::workspace(&root, FilesystemAccess::ReadWrite)
            .expect("absolute workspace");
        let spec = TerminalSessionSpec {
            id: 1,
            actor,
            cwd: SessionPath::new(".").expect("valid cwd"),
            base_grant: grant,
            limits: ResourceLimits::new(1024, 30).expect("valid limits"),
        };
        (spec, root)
    }

    #[test]
    fn cd_persists_for_the_next_command_without_touching_the_host_cwd() {
        let (spec, root) = test_spec(Actor::Human);
        let host_cwd = std::env::current_dir().expect("host cwd");
        let mut session = TerminalSession::new(spec).expect("session opens");

        assert_eq!(session.cwd(), root.as_path());
        session
            .change_dir(&SessionPath::new("sub").expect("valid path"))
            .expect("cd into sub");
        assert_eq!(session.cwd(), root.join("sub"));

        // The host process cwd is untouched.
        assert_eq!(
            std::env::current_dir().expect("host cwd"),
            host_cwd,
            "cd must never change the host process cwd"
        );
    }

    #[test]
    fn cd_rejects_parent_escapes() {
        let (spec, root) = test_spec(Actor::Human);
        let mut session = TerminalSession::new(spec).expect("session opens");
        let outside = root.parent().expect("temp dir has a parent").join("ferrous-session-escape");
        let _ = std::fs::create_dir_all(&outside);

        let result = session.change_dir(&SessionPath::new("../../ferrous-session-escape").expect("lexically valid"));
        assert!(matches!(result, Err(SessionError::PathDenied(_))));
    }

    #[test]
    fn builtin_mkdir_target_resolves_inside_the_workspace_grant() {
        let (spec, root) = test_spec(Actor::Human);
        let session = TerminalSession::new(spec).expect("session opens");
        let target = session
            .resolve_new_path(&SessionPath::new("newdir").expect("valid path"))
            .expect("new path resolves");
        assert!(target.starts_with(&root));
        assert_eq!(target, root.join("newdir"));
    }

    #[test]
    fn env_overlay_only_accepts_allowlisted_names() {
        let (spec, _root) = test_spec(Actor::Agent);
        let mut session = TerminalSession::new(spec).expect("session opens");
        let grant = session.base_grant();
        assert!(!grant.allows_environment("MY_VAR"));
        assert!(matches!(
            session.apply_env(EnvDelta {
                set: BTreeMap::from([("MY_VAR".to_owned(), "x".to_owned())]),
                remove: Vec::new(),
            }),
            Err(SessionError::EnvironmentDenied(_))
        ));
    }

    #[test]
    fn env_never_returns_unallowlisted_host_variables() {
        let (spec, _root) = test_spec(Actor::Human);
        let session = TerminalSession::new(spec).expect("session opens");
        let grant = session.base_grant().clone();
        let session_grant = grant.allow_environment("ALLOWED").expect("valid name");
        let mut session = TerminalSession::new(TerminalSessionSpec {
            base_grant: session_grant,
            ..session.spec().clone()
        })
        .expect("session reopens with allowlist");
        session
            .apply_env(EnvDelta {
                set: BTreeMap::from([("ALLOWED".to_owned(), "yes".to_owned())]),
                remove: Vec::new(),
            })
            .expect("allowlisted name applies");
        assert_eq!(session.env().get("ALLOWED"), Some(&"yes".to_owned()));
        // A host variable that was never allowlisted cannot enter the overlay.
        assert!(session.env().get("PATH").is_none());
    }

    #[test]
    fn session_close_cancels_all_owned_jobs() {
        let (spec, _root) = test_spec(Actor::Subagent);
        let mut session = TerminalSession::new(spec).expect("session opens");
        let first = session
            .register_job("a".to_owned(), crate::cancel::CancelHandle::new())
            .expect("first job");
        let second = session
            .register_job("b".to_owned(), crate::cancel::CancelHandle::new())
            .expect("second job");
        assert_eq!(session.job_count(), 2);

        session.close();
        assert!(session.is_closed());
        assert_eq!(session.job_count(), 0);
        assert!(
            session
                .register_job("c".to_owned(), crate::cancel::CancelHandle::new())
                .is_err(),
            "closed sessions reject new jobs"
        );
        assert!(session.cancel_job(first).is_err());
        assert!(session.cancel_job(second).is_err());
    }

    #[test]
    fn job_table_is_bounded() {
        let (spec, _root) = test_spec(Actor::Skill);
        let mut session = TerminalSession::new(spec).expect("session opens");
        for _ in 0..DEFAULT_MAX_JOBS {
            session
                .register_job("x".to_owned(), crate::cancel::CancelHandle::new())
                .expect("job fits");
        }
        assert!(matches!(
            session.register_job("y".to_owned(), crate::cancel::CancelHandle::new()),
            Err(SessionError::JobTableFull(_))
        ));
    }

    #[test]
    fn cancel_job_removes_it_and_marks_cancelled() {
        let (spec, _root) = test_spec(Actor::Human);
        let mut session = TerminalSession::new(spec).expect("session opens");
        let handle = crate::cancel::CancelHandle::new();
        let id = session
            .register_job("digest".to_owned(), handle.clone())
            .expect("registered");
        assert!(!handle.is_cancelled());
        session.cancel_job(id).expect("cancelled");
        assert!(handle.is_cancelled());
        assert!(matches!(
            session.cancel_job(id),
            Err(SessionError::UnknownJob(_))
        ));
    }
}
