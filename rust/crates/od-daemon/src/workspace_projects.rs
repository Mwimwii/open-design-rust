//! Workspace-scoped project attribution — parity port of the local half of
//! `apps/daemon/src/collab/created-project-workspace.ts` and of the scoped
//! list route `GET /api/workspaces/:workspaceId/projects`
//! (`apps/daemon/src/routes/project/index.ts`).
//!
//! The daemon has no Vela workspace directory, so the authority behind the
//! scoped list is the header seam: exactly the configuration the TypeScript
//! daemon runs in when no `verifyWorkspaceRequestAuthority` is wired
//! (`authoritativeWorkspaceProjectContext` falls back to the same reader).
//!
//! DOCUMENTED GAPS
//!
//! * The remote team-project catalogue (`listRemoteTeamProjectSummaries`) is
//!   not ported: it is a Vela CLI call, and a team workspace therefore lists
//!   only the projects this daemon holds locally.
//! * The workspace type registry (`workspaceTypes.isKnownPersonal`) is not
//!   ported, so only the caller's own `x-od-workspace-type` claim can refuse
//!   a team share.
//! * The group-count telemetry (`workspaceProjectGroupCountProperties`) is
//!   not ported; this daemon reports no workspace analytics.

use serde_json::{json, Value};

use crate::storage::{NewWorkspaceProject, ProjectRow, Store, StoreError, WorkspaceProjectRow};
use crate::workspace_context::{
    is_workspace_locked, WorkspaceMemberStatus, WorkspaceResourceContext, WorkspaceRole,
    WorkspaceType,
};

/// The binding a freshly created project gets (parity:
/// `bindCreatedProjectToWorkspace`).
///
/// INVARIANT: a project created while the daemon knows a signed-in workspace
/// always gets a binding row. A project with no row is not a harmless default
/// — the workspace-scoped route answers `unbound` for it, which strips the
/// workspace off the run request. A headerless request supplies no context
/// and stays unbound.
pub fn bind_created_project_to_workspace(
    store: &Store,
    context: Option<&WorkspaceResourceContext>,
    project_id: &str,
    now: i64,
) -> Result<(), StoreError> {
    let Some(context) = context else {
        return Ok(());
    };
    // This row is local attribution, not publication. It keeps the run
    // routing associated with the captured workspace/member while the project
    // remains `personal` + `local_only`.
    store.ensure_workspace_project(&NewWorkspaceProject {
        project_id,
        workspace_id: &context.workspace_id,
        visibility: "personal",
        resource_state: "active",
        created_by_workspace_member_id: Some(&context.workspace_member_id),
        updated_by_workspace_member_id: Some(&context.workspace_member_id),
        sync_state: "local_only",
        created_at: now,
        updated_at: now,
    })?;
    Ok(())
}

/// Bind a project that belongs to NO workspace to this personal workspace
/// (parity: `bindUnboundProjectsToPersonalWorkspace`).
///
/// The rule is adoption of orphans, not a back-fill of everything: a project
/// that already has a binding is left exactly where it is. Those are the
/// pre-workspace ("legacy") projects, and losing them across the upgrade
/// would be data loss.
pub fn bind_unbound_projects_to_personal_workspace(
    store: &Store,
    context: &WorkspaceResourceContext,
) -> Result<(), StoreError> {
    if context.workspace_type != WorkspaceType::Personal {
        return Ok(());
    }
    for project in store.list_unbound_projects()? {
        ensure_workspace_projection(store, &project, context)?;
    }
    Ok(())
}

/// Project a project into a workspace when it has no binding yet (parity:
/// `ensureWorkspaceProjection`).
///
/// The projected row records no owner: a lazily adopted project belongs to
/// nobody in particular, so a reader is never handed ownership by reading.
pub fn ensure_workspace_projection(
    store: &Store,
    project: &ProjectRow,
    context: &WorkspaceResourceContext,
) -> Result<(), StoreError> {
    store.ensure_workspace_project(&NewWorkspaceProject {
        project_id: &project.id,
        workspace_id: &context.workspace_id,
        visibility: "personal",
        resource_state: "active",
        created_by_workspace_member_id: None,
        updated_by_workspace_member_id: None,
        sync_state: "local_only",
        created_at: project.created_at,
        updated_at: project.updated_at,
    })?;
    Ok(())
}

/// Whether the row belongs to the workspace the request acts in (parity:
/// `workspaceProjectRowBelongsToCurrentWorkspace`).
///
/// A revoked pulled mirror stays bound to its exact team identity as a
/// non-destructive tombstone; it must not appear in any project list while
/// its stale local bytes are quarantined.
pub fn row_belongs_to_current_workspace(
    row: &WorkspaceProjectRow,
    context: &WorkspaceResourceContext,
) -> bool {
    if row.resource_state == "deleted" {
        return false;
    }
    if context.workspace_type != WorkspaceType::Team {
        return true;
    }
    // Legacy rows projected into a team workspace as personal projects with no
    // owner actually belong to the user's personal workspace.
    !(row.workspace_visibility == "personal" && row.created_by_workspace_member_id.is_none())
}

/// Whether the project is visible for the configured locations (parity:
/// `workspaceProjectRowVisibleForLocations`).
///
/// The daemon has no project locations, so a project that names one (the
/// `project-location` import) is not visible: the built-in location never
/// matches it.
pub fn row_visible_for_locations(row: &WorkspaceProjectRow) -> bool {
    metadata_visible_for_locations(row.metadata_json.as_deref())
}

/// The metadata half of [`row_visible_for_locations`] (parity:
/// `projectVisibleForLocations`).
///
/// A project that names a project location is only visible when it belongs to
/// a *configured* location; the built-in one never matches. This daemon has no
/// locations at all, so such a project is not visible.
pub fn metadata_visible_for_locations(metadata_json: Option<&str>) -> bool {
    let metadata = metadata_json
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let from_location =
        metadata.get("importedFrom").and_then(Value::as_str) == Some("project-location");
    let names_location = metadata
        .get("projectLocationId")
        .is_some_and(Value::is_string);
    !(from_location || names_location)
}

/// The access flags of one binding for the caller (parity:
/// `workspaceResourceAccess`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceAccess {
    pub frozen: bool,
    pub self_created: bool,
    pub privileged: bool,
    pub can_mutate: bool,
    pub unattributed: bool,
    pub can_share_local: bool,
    pub disabled_reason: Option<&'static str>,
}

/// The resource-agnostic access computation (parity:
/// `workspaceResourceAccess`).
pub fn resource_access(
    visibility: &str,
    resource_state: &str,
    created_by_member: Option<&str>,
    context: &WorkspaceResourceContext,
) -> ResourceAccess {
    let frozen =
        resource_state == "frozen" || resource_state == "deleted" || is_workspace_locked(context);
    let self_created = created_by_member.is_some()
        && created_by_member == Some(context.workspace_member_id.as_str());
    let privileged = matches!(context.role, WorkspaceRole::Owner | WorkspaceRole::Admin);
    let active = context.member_status == WorkspaceMemberStatus::Active;
    let can_mutate =
        !frozen && context.can_write_synced_files && active && (privileged || self_created);
    let unattributed = created_by_member.is_none();
    let can_share_local = !frozen
        && context.can_write_synced_files
        && active
        && (privileged || self_created || unattributed);
    let disabled_reason = if frozen {
        if context.lifecycle_state == crate::workspace_context::WorkspaceLifecycleState::Deleted
            || resource_state == "deleted"
        {
            Some("workspace_deleted")
        } else {
            Some("workspace_locked")
        }
    } else if can_mutate {
        None
    } else {
        Some("permission_denied")
    };
    let _ = visibility;
    ResourceAccess {
        frozen,
        self_created,
        privileged,
        can_mutate,
        unattributed,
        can_share_local,
        disabled_reason,
    }
}

/// The project-specific affordances the list hands the client (parity:
/// `projectAccess`).
pub fn project_access(row: &WorkspaceProjectRow, context: &WorkspaceResourceContext) -> Value {
    let access = resource_access(
        &row.workspace_visibility,
        &row.resource_state,
        row.created_by_workspace_member_id.as_deref(),
        context,
    );
    // Team-shared projects are single-writer resources: only the member
    // recorded as the creator may mutate or unshare one.
    let can_mutate =
        access.can_mutate && (row.workspace_visibility != "team" || access.self_created);
    let disabled_reason = access.disabled_reason.or(if can_mutate {
        None
    } else {
        Some("permission_denied")
    });
    let active = context.member_status == WorkspaceMemberStatus::Active;
    // A team share must live in a team workspace. Without the Vela directory
    // the only witness is the caller's own `x-od-workspace-type` claim.
    let asserted_personal = context.workspace_type_asserted == Some(WorkspaceType::Personal);
    let can_move_to_team = access.can_share_local
        && context.can_share_projects
        && row.workspace_visibility == "personal"
        && !asserted_personal;
    let can_move_to_personal =
        can_mutate && context.can_share_projects && row.workspace_visibility == "team";
    let mut value = json!({
        "canOpen": !access.frozen && active,
        "canRename": can_mutate,
        "canDelete": can_mutate,
        "canDuplicate": can_mutate,
        "canMoveToTeam": can_move_to_team,
        "canMoveToPersonal": can_move_to_personal,
        "canExport": !access.frozen && active,
        "canSendTo": !access.frozen && active,
        "canRestoreVersion": can_mutate,
    });
    if let Some(reason) = disabled_reason {
        value["disabledReason"] = Value::String(reason.to_string());
    }
    value
}

/// The summary the scoped list returns for one row (parity:
/// `normalizeWorkspaceProjectRow`).
pub fn normalize_workspace_project_row(
    row: &WorkspaceProjectRow,
    context: &WorkspaceResourceContext,
) -> Value {
    let metadata = row
        .metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    // A move/rename/share-visibility change touches only the binding row, but
    // it is real recent activity: report the later of the two.
    let last_activity_at = row.updated_at.max(row.workspace_updated_at);
    let resource_state = if is_workspace_locked(context) && row.workspace_visibility == "team" {
        "frozen"
    } else {
        row.resource_state.as_str()
    };
    let mut project = json!({
        "id": row.id,
        "name": row.name,
        "skillId": row.skill_id,
        "designSystemId": row.design_system_id,
        "createdAt": row.created_at,
        "updatedAt": last_activity_at,
        // Carried on the nested project too, so a client that unwraps the
        // summary into a plain project keeps the binding.
        "workspaceId": row.workspace_id,
    });
    if let Some(pending_prompt) = &row.pending_prompt {
        project["pendingPrompt"] = Value::String(pending_prompt.clone());
    }
    if let Some(snapshot) = &row.applied_plugin_snapshot_id {
        project["appliedPluginSnapshotId"] = Value::String(snapshot.clone());
    }
    if let Some(custom_instructions) = &row.custom_instructions {
        project["customInstructions"] = Value::String(custom_instructions.clone());
    }
    if let Some(metadata) = &metadata {
        project["metadata"] = metadata.clone();
    }
    let mut summary = json!({
        "id": row.id,
        "name": row.name,
        "workspaceId": row.workspace_id,
        "visibility": row.workspace_visibility,
        "resourceState": resource_state,
        "createdByWorkspaceMemberId": row.created_by_workspace_member_id,
        "updatedByWorkspaceMemberId": row.updated_by_workspace_member_id,
        "resourceHubResourceId": row.resource_hub_resource_id,
        "cloudTombstonedAt": row.cloud_tombstoned_at,
        "currentUserAccess": project_access(row, context),
        "syncState": row.sync_state.clone().unwrap_or_else(|| "local_only".to_string()),
        "createdAt": row.created_at,
        "updatedAt": last_activity_at,
        "project": project,
    });
    if let Some(metadata) = &metadata {
        summary["metadata"] = metadata.clone();
    }
    if row.sync_state.as_deref() == Some("pending_upload") {
        summary["pendingSyncIntent"] = json!({
            "event": if row.workspace_visibility == "team" {
                "project_team_share_requested"
            } else {
                "project_team_unshare_requested"
            },
            "projectId": row.id,
            "workspaceId": row.workspace_id,
        });
    }
    summary
}

/// Whether the caller created the project (parity:
/// `workspaceProjectCreatedByCurrentMember`).
pub fn created_by_current_member(summary: &Value, context: &WorkspaceResourceContext) -> bool {
    let creator = summary
        .get("createdByWorkspaceMemberId")
        .and_then(Value::as_str);
    if creator == Some(context.workspace_member_id.as_str()) {
        return true;
    }
    // A personal project with no owner is the caller's own: it was adopted by
    // this personal workspace.
    context.workspace_type == WorkspaceType::Personal
        && summary.get("visibility").and_then(Value::as_str) == Some("personal")
        && creator.is_none()
}

/// The list filters (parity: the `projects.filter(...)` block of the scoped
/// list route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListFilters {
    pub view: String,
    pub owner: String,
    pub visibility: String,
}

impl ListFilters {
    /// Whether a summary passes the filters.
    pub fn keeps(&self, summary: &Value, context: &WorkspaceResourceContext) -> bool {
        let created_by_current_member = created_by_current_member(summary, context);
        let visibility = summary
            .get("visibility")
            .and_then(Value::as_str)
            .unwrap_or("");
        // Personal projects are private to their recorded creator, even when
        // two accounts have used the same local data root.
        if visibility == "personal" && !created_by_current_member {
            return false;
        }
        if self.view == "drafts" && (visibility != "personal" || !created_by_current_member) {
            return false;
        }
        if self.view == "team" && visibility != "team" {
            return false;
        }
        if (self.visibility == "personal" || self.visibility == "team")
            && visibility != self.visibility
        {
            return false;
        }
        if self.owner == "mine" && !created_by_current_member {
            return false;
        }
        if self.owner == "others" && created_by_current_member {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_context::{
        WorkspaceLifecycleState, WorkspaceMemberStatus, WorkspaceRole, WorkspaceType,
    };

    fn member_context() -> WorkspaceResourceContext {
        WorkspaceResourceContext {
            workspace_id: "w-1".to_string(),
            workspace_type: WorkspaceType::Personal,
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

    fn row() -> WorkspaceProjectRow {
        WorkspaceProjectRow {
            id: "p-1".to_string(),
            name: "Deck".to_string(),
            skill_id: None,
            design_system_id: None,
            pending_prompt: None,
            metadata_json: Some("{\"kind\":\"prototype\"}".to_string()),
            applied_plugin_snapshot_id: None,
            custom_instructions: None,
            created_at: 10,
            updated_at: 20,
            workspace_id: "w-1".to_string(),
            workspace_visibility: "personal".to_string(),
            resource_state: "active".to_string(),
            created_by_workspace_member_id: Some("m-1".to_string()),
            updated_by_workspace_member_id: Some("m-1".to_string()),
            resource_hub_resource_id: None,
            cloud_tombstoned_at: None,
            sync_state: Some("local_only".to_string()),
            workspace_version: 1,
            workspace_created_at: 10,
            workspace_updated_at: 15,
        }
    }

    #[test]
    fn the_summary_carries_the_project_and_the_access() {
        let summary = normalize_workspace_project_row(&row(), &member_context());
        assert_eq!(summary["id"], "p-1");
        assert_eq!(summary["workspaceId"], "w-1");
        assert_eq!(summary["visibility"], "personal");
        assert_eq!(summary["metadata"]["kind"], "prototype");
        assert_eq!(summary["project"]["workspaceId"], "w-1");
        // The binding was touched after the project: the later stamp wins.
        assert_eq!(summary["updatedAt"], 20);
        assert_eq!(summary["currentUserAccess"]["canRename"], true);
        assert_eq!(summary["currentUserAccess"]["canOpen"], true);
    }

    #[test]
    fn another_members_project_is_read_only() {
        let mut row = row();
        row.created_by_workspace_member_id = Some("m-2".to_string());
        let summary = normalize_workspace_project_row(&row, &member_context());
        assert_eq!(summary["currentUserAccess"]["canRename"], false);
        assert_eq!(
            summary["currentUserAccess"]["disabledReason"],
            "permission_denied"
        );
        // The personal project of another member is private.
        let filters = ListFilters {
            view: "all".to_string(),
            owner: "all".to_string(),
            visibility: "all".to_string(),
        };
        assert!(!filters.keeps(&summary, &member_context()));
    }

    #[test]
    fn a_removed_member_sees_nothing() {
        let mut context = member_context();
        context.member_status = WorkspaceMemberStatus::Removed;
        let summary = normalize_workspace_project_row(&row(), &context);
        assert_eq!(summary["currentUserAccess"]["canOpen"], false);
    }

    #[test]
    fn a_locked_workspace_freezes_its_team_projects() {
        let mut context = member_context();
        context.workspace_type = WorkspaceType::Team;
        context.lifecycle_state = WorkspaceLifecycleState::Locked;
        let mut row = row();
        row.workspace_visibility = "team".to_string();
        let summary = normalize_workspace_project_row(&row, &context);
        assert_eq!(summary["resourceState"], "frozen");
        assert_eq!(summary["currentUserAccess"]["canOpen"], false);
    }

    #[test]
    fn a_team_workspace_hides_the_unowned_personal_rows() {
        let mut context = member_context();
        context.workspace_type = WorkspaceType::Team;
        let mut row = row();
        row.created_by_workspace_member_id = None;
        assert!(!row_belongs_to_current_workspace(&row, &context));
        row.workspace_visibility = "team".to_string();
        assert!(row_belongs_to_current_workspace(&row, &context));
    }

    #[test]
    fn the_team_share_needs_a_workspace_that_can_host_one() {
        let mut context = member_context();
        context.workspace_type_asserted = Some(WorkspaceType::Personal);
        let summary = normalize_workspace_project_row(&row(), &context);
        assert_eq!(summary["currentUserAccess"]["canMoveToTeam"], false);
        context.workspace_type_asserted = None;
        let summary = normalize_workspace_project_row(&row(), &context);
        assert_eq!(summary["currentUserAccess"]["canMoveToTeam"], true);
    }
}
