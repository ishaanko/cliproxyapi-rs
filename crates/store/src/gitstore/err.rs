//! Error type of the git store internals. Go matches sentinel errors with `errors.Is`; here each
//! error carries a small set of kind flags that survive `wrap` and `join`.

use std::fmt;

use git2::{ErrorClass, ErrorCode};

pub(super) const EMPTY_REMOTE: u16 = 1;
pub(super) const AUTH_REQUIRED: u16 = 1 << 1;
pub(super) const REF_NOT_FOUND: u16 = 1 << 2;
/// Missing object or pack (go-git `ErrObjectNotFound` / `dotgit.ErrPackfileNotFound`).
pub(super) const OBJECT_NOT_FOUND: u16 = 1 << 3;
pub(super) const UNSTAGED_CHANGES: u16 = 1 << 4;
pub(super) const NON_FAST_FORWARD: u16 = 1 << 5;
pub(super) const UP_TO_DATE: u16 = 1 << 6;

#[derive(Debug, Clone)]
pub(super) struct GitErr {
    msg: String,
    kinds: u16,
}

pub(super) type R<T> = Result<T, GitErr>;

impl GitErr {
    pub(super) fn msg(msg: impl Into<String>) -> Self {
        Self { msg: msg.into(), kinds: 0 }
    }

    /// The sentinel error for `kind`, with go-git's wording.
    pub(super) fn kind(kind: u16) -> Self {
        let msg = match kind {
            EMPTY_REMOTE => "remote repository is empty",
            AUTH_REQUIRED => "authentication required",
            REF_NOT_FOUND => "reference not found",
            OBJECT_NOT_FOUND => "object not found",
            UNSTAGED_CHANGES => "worktree contains unstaged changes",
            NON_FAST_FORWARD => "non-fast-forward update",
            UP_TO_DATE => "already up-to-date",
            _ => "git error",
        };
        Self { msg: msg.into(), kinds: kind }
    }

    pub(super) fn is(&self, kind: u16) -> bool {
        self.kinds & kind != 0
    }

    /// `isRepositoryCorruptionError`.
    pub(super) fn is_corruption(&self) -> bool {
        self.is(OBJECT_NOT_FOUND)
    }

    /// `fmt.Errorf("%s: %w", prefix, err)`.
    pub(super) fn wrap(self, prefix: impl fmt::Display) -> Self {
        Self { msg: format!("{prefix}: {}", self.msg), kinds: self.kinds }
    }

    /// `errors.Join(self, other)`.
    pub(super) fn join(self, other: GitErr) -> Self {
        Self { msg: format!("{}\n{}", self.msg, other.msg), kinds: self.kinds | other.kinds }
    }

    pub(super) fn into_message(self) -> String {
        self.msg
    }
}

impl fmt::Display for GitErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<git2::Error> for GitErr {
    fn from(e: git2::Error) -> Self {
        match (e.code(), e.class()) {
            (ErrorCode::UnbornBranch, _) | (ErrorCode::NotFound, ErrorClass::Reference) => {
                GitErr::kind(REF_NOT_FOUND)
            }
            (ErrorCode::NotFound, ErrorClass::Odb | ErrorClass::Object | ErrorClass::Tree) => {
                GitErr { msg: format!("object not found: {}", e.message()), kinds: OBJECT_NOT_FOUND }
            }
            (ErrorCode::Auth, _) => GitErr::kind(AUTH_REQUIRED),
            _ => GitErr::msg(e.message().to_string()),
        }
    }
}

impl From<std::io::Error> for GitErr {
    fn from(e: std::io::Error) -> Self {
        GitErr::msg(e.to_string())
    }
}
