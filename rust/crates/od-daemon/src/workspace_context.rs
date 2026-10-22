//! Workspace attribution carried by the request headers — parity port of
//! `apps/daemon/src/collab/workspace-resource-mutation.ts` (the
//! `WorkspaceResourceContext` shape and the two header readers).
//!
//! The daemon has no Vela workspace directory, so the *legacy* header seam is
//! the only authority it can offer. That is exactly the configuration the
//! TypeScript daemon runs in when no `verifyWorkspaceRequestAuthority` is
//! wired: `authoritativeWorkspaceProjectContext` falls back to the same header
//! reader (`workspaceResourceContext(req, expectedWorkspaceId)`).
//!
//! DOCUMENTED GAPS
//!
//! * The Vela authority check itself (`verifyWorkspaceRequestAuthority`) is not
//!   ported, so a caller that asserts a workspace is trusted the way the
//!   TypeScript daemon trusts a header-only request.
//! * The workspace *type registry* (`workspaceTypes.isKnownPersonal`) is not
//!   ported, so only the caller's own `x-od-workspace-type` claim can refuse a
//!   team share.

use axum::http::HeaderMap;

/// The kind of workspace a request acts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceType {
    Personal,
    Team,
}

impl WorkspaceType {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceType::Personal => "personal",
            WorkspaceType::Team => "team",
        }
    }
}

/// The caller's role in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRole {
    Owner,
    Admin,
    Member,
}

impl WorkspaceRole {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceRole::Owner => "owner",
            WorkspaceRole::Admin => "admin",
            WorkspaceRole::Member => "member",
        }
    }
}

/// Whether the member is still in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMemberStatus {
    Active,
    Removed,
}

/// The workspace lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceLifecycleState {
    Active,
    BillingPastDue,
    Locked,
    Deleting,
    Deleted,
}

impl WorkspaceLifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceLifecycleState::Active => "active",
            WorkspaceLifecycleState::BillingPastDue => "billing_past_due",
            WorkspaceLifecycleState::Locked => "locked",
            WorkspaceLifecycleState::Deleting => "deleting",
            WorkspaceLifecycleState::Deleted => "deleted",
        }
    }
}

/// The workspace a request acts in (parity: `WorkspaceResourceContext`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceResourceContext {
    pub workspace_id: String,
    pub workspace_type: WorkspaceType,
    /// The kind the caller *asserted*, when it said one.
    pub workspace_type_asserted: Option<WorkspaceType>,
    pub app_user_id: String,
    pub workspace_member_id: String,
    pub role: WorkspaceRole,
    pub member_status: WorkspaceMemberStatus,
    pub lifecycle_state: WorkspaceLifecycleState,
    pub can_share_projects: bool,
    pub can_write_synced_files: bool,
}

/// What a request claims about its workspace.
///
/// The TypeScript reader answers `'missing'` when exactly one of the two
/// identity headers is present: the request names a workspace it cannot
/// identify itself as, so there is no context to work with (but the request
/// is not headerless either).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceAssertion {
    /// The request named no workspace at all.
    None,
    /// The request named a workspace but not a member (or the other way round).
    Missing,
    /// The request named both.
    Complete(WorkspaceResourceContext),
}

/// A trimmed header value (parity: `headerValue`).
pub fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// A boolean header with a fallback (parity: `headerBool`).
pub fn header_bool(headers: &HeaderMap, name: &str, fallback: bool) -> bool {
    match header_value(headers, name).as_deref() {
        None => fallback,
        Some("false") => false,
        Some("true") => true,
        Some(_) => fallback,
    }
}

/// The workspace context for a request that names one (parity:
/// `workspaceResourceContext`).
///
/// The workspace id comes from the caller, not from a header: the route
/// decides which workspace the request is about.
pub fn workspace_resource_context(
    headers: &HeaderMap,
    workspace_id: &str,
) -> Option<WorkspaceResourceContext> {
    let workspace_member_id = header_value(headers, "x-od-workspace-member-id")?;
    let workspace_type_header = header_value(headers, "x-od-workspace-type");
    let workspace_type = match workspace_type_header.as_deref() {
        Some("team") => WorkspaceType::Team,
        _ => WorkspaceType::Personal,
    };
    let workspace_type_asserted = match workspace_type_header.as_deref() {
        Some("team") => Some(WorkspaceType::Team),
        Some("personal") => Some(WorkspaceType::Personal),
        _ => None,
    };
    let role = match header_value(headers, "x-od-workspace-role").as_deref() {
        Some("owner") => WorkspaceRole::Owner,
        Some("admin") => WorkspaceRole::Admin,
        _ => WorkspaceRole::Member,
    };
    let member_status = match header_value(headers, "x-od-workspace-member-status").as_deref() {
        Some("removed") => WorkspaceMemberStatus::Removed,
        _ => WorkspaceMemberStatus::Active,
    };
    let lifecycle_state = match header_value(headers, "x-od-workspace-lifecycle-state").as_deref() {
        Some("billing_past_due") => WorkspaceLifecycleState::BillingPastDue,
        Some("locked") => WorkspaceLifecycleState::Locked,
        Some("deleting") => WorkspaceLifecycleState::Deleting,
        Some("deleted") => WorkspaceLifecycleState::Deleted,
        _ => WorkspaceLifecycleState::Active,
    };
    let legacy_write_enabled = header_bool(headers, "x-od-workspace-write-enabled", true);
    let can_write_synced_files = header_bool(
        headers,
        "x-od-workspace-can-write-synced-files",
        legacy_write_enabled,
    );
    Some(WorkspaceResourceContext {
        workspace_id: workspace_id.to_string(),
        workspace_type,
        workspace_type_asserted,
        app_user_id: header_value(headers, "x-od-app-user-id")
            .unwrap_or_else(|| "local-user".to_string()),
        workspace_member_id,
        role,
        member_status,
        lifecycle_state,
        can_share_projects: header_bool(
            headers,
            "x-od-workspace-can-share-projects",
            can_write_synced_files,
        ),
        can_write_synced_files,
    })
}

/// The workspace a request asserts (parity:
/// `workspaceResourceContextFromRequest`).
pub fn workspace_resource_context_from_request(headers: &HeaderMap) -> WorkspaceAssertion {
    let workspace_id = header_value(headers, "x-od-workspace-id");
    let workspace_member_id = header_value(headers, "x-od-workspace-member-id");
    let (Some(workspace_id), Some(_)) = (&workspace_id, &workspace_member_id) else {
        if workspace_id.is_none() && workspace_member_id.is_none() {
            return WorkspaceAssertion::None;
        }
        return WorkspaceAssertion::Missing;
    };
    match workspace_resource_context(headers, workspace_id) {
        Some(context) => WorkspaceAssertion::Complete(context),
        None => WorkspaceAssertion::Missing,
    }
}

/// Whether the workspace is locked or deleted (parity:
/// `isWorkspaceResourceLocked`).
pub fn is_workspace_locked(context: &WorkspaceResourceContext) -> bool {
    matches!(
        context.lifecycle_state,
        WorkspaceLifecycleState::Locked | WorkspaceLifecycleState::Deleted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        headers
    }

    #[test]
    fn a_headerless_request_asserts_no_workspace() {
        assert_eq!(
            workspace_resource_context_from_request(&headers(&[])),
            WorkspaceAssertion::None
        );
    }

    #[test]
    fn half_a_workspace_identity_is_missing() {
        assert_eq!(
            workspace_resource_context_from_request(&headers(&[("x-od-workspace-id", "w-1")])),
            WorkspaceAssertion::Missing
        );
        assert_eq!(
            workspace_resource_context_from_request(&headers(&[(
                "x-od-workspace-member-id",
                "m-1"
            )])),
            WorkspaceAssertion::Missing
        );
    }

    #[test]
    fn the_context_reads_every_header() {
        let headers = headers(&[
            ("x-od-workspace-id", " w-1 "),
            ("x-od-workspace-member-id", "m-1"),
            ("x-od-workspace-type", "team"),
            ("x-od-workspace-role", "admin"),
            ("x-od-workspace-member-status", "removed"),
            ("x-od-workspace-lifecycle-state", "locked"),
            ("x-od-app-user-id", "u-1"),
            ("x-od-workspace-can-share-projects", "false"),
            ("x-od-workspace-can-write-synced-files", "true"),
        ]);
        let WorkspaceAssertion::Complete(context) =
            workspace_resource_context_from_request(&headers)
        else {
            panic!("a complete identity must produce a context");
        };
        assert_eq!(context.workspace_id, "w-1");
        assert_eq!(context.workspace_member_id, "m-1");
        assert_eq!(context.workspace_type, WorkspaceType::Team);
        assert_eq!(context.workspace_type_asserted, Some(WorkspaceType::Team));
        assert_eq!(context.role, WorkspaceRole::Admin);
        assert_eq!(context.member_status, WorkspaceMemberStatus::Removed);
        assert_eq!(context.lifecycle_state, WorkspaceLifecycleState::Locked);
        assert_eq!(context.app_user_id, "u-1");
        assert!(!context.can_share_projects);
        assert!(context.can_write_synced_files);
        assert!(is_workspace_locked(&context));
    }

    #[test]
    fn an_untyped_workspace_is_personal_without_an_assertion() {
        let headers = headers(&[
            ("x-od-workspace-id", "w-1"),
            ("x-od-workspace-member-id", "m-1"),
        ]);
        let WorkspaceAssertion::Complete(context) =
            workspace_resource_context_from_request(&headers)
        else {
            panic!("a complete identity must produce a context");
        };
        assert_eq!(context.workspace_type, WorkspaceType::Personal);
        assert_eq!(context.workspace_type_asserted, None);
        assert_eq!(context.role, WorkspaceRole::Member);
        assert_eq!(context.member_status, WorkspaceMemberStatus::Active);
        assert_eq!(context.lifecycle_state, WorkspaceLifecycleState::Active);
        assert_eq!(context.app_user_id, "local-user");
        // The share ceiling follows the write ceiling when it is not stated.
        assert!(context.can_write_synced_files);
        assert!(context.can_share_projects);
    }

    #[test]
    fn a_removed_member_still_reads_the_workspace() {
        // The reader does not refuse a removed member: the routes decide what
        // a removed member may see.
        let headers = headers(&[
            ("x-od-workspace-id", "w-1"),
            ("x-od-workspace-member-id", "m-1"),
            ("x-od-workspace-member-status", "removed"),
        ]);
        let WorkspaceAssertion::Complete(context) =
            workspace_resource_context_from_request(&headers)
        else {
            panic!("a complete identity must produce a context");
        };
        assert_eq!(context.member_status, WorkspaceMemberStatus::Removed);
    }
}
