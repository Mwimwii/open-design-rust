//! Moving a project between personal and team visibility inside a workspace
//! — parity port of the local half of `POST
//! /api/workspaces/:workspaceId/projects/:projectId/move` and of its
//! `batch-move` sibling (`apps/daemon/src/routes/project/index.ts`).
//!
//! DOCUMENTED GAPS
//!
//! * The remote team share/unshare (`collabSync.requestTeamShare` /
//!   `requestTeamUnshare`) is not ported: it is a Vela CLI call. A team move
//!   therefore leaves the binding in `pending_upload` — exactly the state the
//!   TypeScript daemon writes before the remote call — and the summary
//!   reports the `project_team_share_requested` intent.
//! * The team-project catalogue reconciliation
//!   (`reconcileUnboundProjectBeforeMove`) is not ported either, so a "move
//!   to personal" of a project no workspace has claimed is refused with
//!   `UPSTREAM_UNAVAILABLE` rather than reconciled against the catalogue.
//! * The team comment anchors (`ensureTeamProjectCommentConversations`) are
//!   not ported: those conversations belong to the remote share.

use base64::Engine;
use serde_json::{json, Value};

use crate::storage::WorkspaceProjectUpdate;
use crate::workspace_context::{
    WorkspaceLifecycleState, WorkspaceMemberStatus, WorkspaceResourceContext, WorkspaceType,
};

/// The visibility of a project that belongs to one person (parity:
/// `visibility` in the `workspace_projects` row).
pub const PERSONAL: &str = "personal";

/// The visibility of a project shared with a team (parity: `visibility` in
/// the `workspace_projects` row).
pub const TEAM: &str = "team";

/// Whether the value is a visibility a move may target (parity:
/// `validVisibility`).
pub fn is_valid_visibility(value: &str) -> bool {
    value == PERSONAL || value == TEAM
}

/// Why a team share may not be recorded in this workspace (parity:
/// `refuseTeamShareScope`).
///
/// The workspace *type registry* is not ported, so the only evidence against
/// a team share is the caller's own `x-od-workspace-type` claim: a caller
/// that says "personal" and asks for a team share has stated the
/// contradiction itself.
pub fn team_share_refusal(context: &WorkspaceResourceContext) -> Option<&'static str> {
    if context.workspace_type_asserted == Some(WorkspaceType::Personal) {
        Some("asserted_personal")
    } else {
        None
    }
}

/// The resource-hub id a shared project is published under (parity:
/// `projectResourceIdFor`).
///
/// The id is scoped to the team, the member and the project, so two members
/// of one team never claim the same published resource.
pub fn project_resource_id_for(project_id: &str, context: &WorkspaceResourceContext) -> String {
    let scoped = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_string(&json!([
            context.workspace_id,
            context.workspace_member_id,
            project_id
        ]))
        .unwrap_or_default(),
    );
    format!("project-{scoped}")
}

/// Whether the caller may move the project to the target visibility (parity:
/// `workspaceMoveAllowed`).
pub fn move_allowed(summary: &Value, target: &str, context: &WorkspaceResourceContext) -> bool {
    if move_retry_allowed(summary, target, context) {
        return true;
    }
    let access = summary.get("currentUserAccess");
    let flag = if target == TEAM {
        "canMoveToTeam"
    } else {
        "canMoveToPersonal"
    };
    access
        .and_then(|value| value.get(flag))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the member that owns a failed share may retry it (parity:
/// `workspaceMoveRetryAllowed`).
///
/// A failed publish has already crossed the local visibility boundary, so
/// `canMoveToTeam` is deliberately false for it. Only the exact member still
/// recorded as the share's owner may retry the *same* team target.
fn move_retry_allowed(summary: &Value, target: &str, context: &WorkspaceResourceContext) -> bool {
    target == TEAM
        && summary.get("visibility").and_then(Value::as_str) == Some(TEAM)
        && summary.get("syncState").and_then(Value::as_str) == Some("sync_failed")
        && summary
            .get("createdByWorkspaceMemberId")
            .and_then(Value::as_str)
            == Some(context.workspace_member_id.as_str())
        && summary
            .get("currentUserAccess")
            .and_then(|value| value.get("canRename"))
            .and_then(Value::as_bool)
            == Some(true)
        && context.can_share_projects
        && context.member_status == WorkspaceMemberStatus::Active
        && context.lifecycle_state == WorkspaceLifecycleState::Active
}

/// The binding patch a move writes (parity: `workspaceProjectMovePatch`).
///
/// A team move names the member that owns the share, points the row at the
/// published resource and asks for the upload; a move back to personal drops
/// the resource id, tombstones the cloud copy and returns the row to
/// `local_only`.
pub fn move_patch(
    project_id: &str,
    summary: &Value,
    context: &WorkspaceResourceContext,
    visibility: &str,
    now: i64,
) -> WorkspaceProjectUpdate {
    let owner = summary
        .get("createdByWorkspaceMemberId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let created_by = if visibility == TEAM {
        owner.or_else(|| Some(context.workspace_member_id.clone()))
    } else {
        owner
    };
    WorkspaceProjectUpdate {
        visibility: visibility.to_string(),
        created_by_workspace_member_id: created_by,
        updated_by_workspace_member_id: Some(context.workspace_member_id.clone()),
        resource_hub_resource_id: if visibility == TEAM {
            Some(project_resource_id_for(project_id, context))
        } else {
            None
        },
        cloud_tombstoned_at: if visibility == TEAM { None } else { Some(now) },
        sync_state: if visibility == TEAM {
            "pending_upload".to_string()
        } else {
            "local_only".to_string()
        },
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_context::{WorkspaceRole, WorkspaceType};

    fn member_context() -> WorkspaceResourceContext {
        WorkspaceResourceContext {
            workspace_id: "w-1".to_string(),
            workspace_type: WorkspaceType::Team,
            workspace_type_asserted: None,
            app_user_id: "local-user".to_string(),
            workspace_member_id: "m-1".to_string(),
            role: WorkspaceRole::Member,
            member_status: WorkspaceMemberStatus::Active,
            lifecycle_state: WorkspaceLifecycleState::Active,
            can_share_projects: true,
            can_write_synced_files: true,
        }
    }

    fn summary(visibility: &str, sync_state: &str) -> Value {
        json!({
            "id": "p-1",
            "visibility": visibility,
            "syncState": sync_state,
            "createdByWorkspaceMemberId": "m-1",
            "currentUserAccess": {
                "canRename": true,
                "canMoveToTeam": visibility == PERSONAL,
                "canMoveToPersonal": visibility == TEAM,
            }
        })
    }

    #[test]
    fn a_move_needs_a_known_visibility() {
        assert!(is_valid_visibility(PERSONAL));
        assert!(is_valid_visibility(TEAM));
        assert!(!is_valid_visibility(""));
        assert!(!is_valid_visibility("draft"));
    }

    #[test]
    fn the_resource_id_is_scoped_to_the_team_and_member() {
        let context = member_context();
        let id = project_resource_id_for("p-1", &context);
        // base64url of ["w-1","m-1","p-1"] without padding.
        assert_eq!(id, "project-WyJ3LTEiLCJtLTEiLCJwLTEiXQ");
        assert!(!id.contains('='));
    }

    #[test]
    fn a_personal_claim_refuses_a_team_share() {
        let mut context = member_context();
        assert_eq!(team_share_refusal(&context), None);
        context.workspace_type_asserted = Some(WorkspaceType::Personal);
        assert_eq!(team_share_refusal(&context), Some("asserted_personal"));
    }

    #[test]
    fn the_access_flags_decide_the_move() {
        let context = member_context();
        assert!(move_allowed(
            &summary(PERSONAL, "local_only"),
            TEAM,
            &context
        ));
        assert!(move_allowed(
            &summary(TEAM, "local_only"),
            PERSONAL,
            &context
        ));
        // The wrong direction is refused.
        assert!(!move_allowed(
            &summary(PERSONAL, "local_only"),
            PERSONAL,
            &context
        ));
        assert!(!move_allowed(&summary(TEAM, "local_only"), TEAM, &context));
    }

    #[test]
    fn the_owner_of_a_failed_share_may_retry_it() {
        let context = member_context();
        let mut failed = summary(TEAM, "sync_failed");
        // A failed share is not movable by the generic flag.
        failed["currentUserAccess"]["canMoveToTeam"] = json!(false);
        assert!(move_allowed(&failed, TEAM, &context));
        // ... but not by anybody else.
        let mut other = member_context();
        other.workspace_member_id = "m-2".to_string();
        assert!(!move_allowed(&failed, TEAM, &other));
    }

    #[test]
    fn the_team_patch_asks_for_the_upload() {
        let context = member_context();
        let patch = move_patch("p-1", &summary(PERSONAL, "local_only"), &context, TEAM, 42);
        assert_eq!(patch.visibility, TEAM);
        assert_eq!(patch.created_by_workspace_member_id.as_deref(), Some("m-1"));
        assert_eq!(patch.updated_by_workspace_member_id.as_deref(), Some("m-1"));
        assert!(patch.resource_hub_resource_id.is_some());
        assert!(patch.cloud_tombstoned_at.is_none());
        assert_eq!(patch.sync_state, "pending_upload");
        assert_eq!(patch.updated_at, 42);
    }

    #[test]
    fn the_personal_patch_tombstones_the_cloud_copy() {
        let context = member_context();
        let patch = move_patch("p-1", &summary(TEAM, "synced"), &context, PERSONAL, 42);
        assert_eq!(patch.visibility, PERSONAL);
        assert_eq!(patch.created_by_workspace_member_id.as_deref(), Some("m-1"));
        assert!(patch.resource_hub_resource_id.is_none());
        assert_eq!(patch.cloud_tombstoned_at, Some(42));
        assert_eq!(patch.sync_state, "local_only");
    }
}
