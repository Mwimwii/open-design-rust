//! Project patch helpers — parity port of the pure parts of
//! `PATCH /api/projects/:id` (`apps/daemon/src/routes/project/index.ts:5078`).
//!
//! The route's decision logic is the *privileged* part: which metadata fields
//! the daemon owns, which are immutable after an import, and which a patch may
//! set at all. It lives here so the route stays thin and every rule is unit
//! tested.
//!
//! DOCUMENTED GAPS
//!
//! * The workspace mutation gate (`enforceWorkspaceProjectMutation`) is not
//!   ported: this daemon has no workspace identity, so a patch is never
//!   workspace-scoped and no member check can run.
//! * The team-catalog rename propagation (`propagateWorkspaceProjectRename`)
//!   is not ported: a rename rewrites the project row only, because the daemon
//!   has no user design-system registry bound to a workspace.
//! * The team-catalog metadata refresh (`collabSync.refreshTeamProjectMetadata`)
//!   is not ported for the same reason.
//! * A non-object `metadata` body is refused with a 400. The TypeScript route
//!   stores it verbatim (`updateProject` stringifies whatever it is handed);
//!   refusing keeps the metadata document an object.
//! * The daemon-owned binding comparisons are value comparisons, not
//!   `JSON.stringify` comparisons, so a patch that repeats a daemon-owned
//!   binding with a different key order is accepted instead of refused. The
//!   daemon re-stamps the stored value either way, so no data changes.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::project_create::{parse_orchestrator_workspace, validate_linked_dirs};
use crate::storage::{ProjectRow, ProjectUpdate};

/// The sentinel `updatedAt` value that keeps the existing stamp (parity:
/// `SYNC_KEEPS_UPDATED_AT`).
const SYNC_KEEPS_UPDATED_AT: &str = "__od.sync-keeps-updated-at__";

/// The 400 a patch answers with (parity: the `sendApiError` calls in the
/// TypeScript route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchError {
    /// The API error code.
    pub code: String,
    /// The API error message.
    pub message: String,
}

impl PatchError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            code: "BAD_REQUEST".to_string(),
            message: message.into(),
        }
    }
}

/// The catalogue ids a patch may reference (parity: the `listAllSkills` /
/// `listAllDesignSystems` reads behind the TypeScript validators).
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    /// Every skill id the local catalogue knows.
    pub skills: HashSet<String>,
    /// Every design-system id the local catalogue knows, with its status
    /// (`published` / `draft`).
    pub design_systems: HashMap<String, String>,
}

/// JavaScript truthiness (parity: the `if (value)` checks in the TypeScript
/// route).
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// Two JSON values compared by content, with "absent" distinct from "null"
/// (parity: `JSON.stringify(a) === JSON.stringify(b)` over `undefined`).
fn same_json(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Two orchestrator workspaces compared after parsing (parity:
/// `sameOrchestratorWorkspace`).
fn same_orchestrator_workspace(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (
        parse_orchestrator_workspace(left),
        parse_orchestrator_workspace(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// The metadata document the stored row carries (parity: the parsed
/// `metadata_json` column).
fn existing_metadata(existing: &ProjectRow) -> Value {
    existing
        .metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null)
}

/// Compose the row a patch writes (parity: the guard block the TypeScript
/// route runs before it calls `updateProject`).
///
/// The result is the *merged* row: a patch only replaces the fields it names,
/// and the daemon-owned metadata survives a patch that omits it.
pub fn compose_project_patch(
    existing: &ProjectRow,
    body: &Value,
    catalogue: &Catalogue,
    now: i64,
) -> Result<ProjectUpdate, PatchError> {
    let Some(patch) = body.as_object() else {
        return Err(PatchError::bad_request("patch body must be an object"));
    };
    let existing_metadata = existing_metadata(existing);

    // ---- metadata --------------------------------------------------------
    let mut metadata: Option<Value> = match patch.get("metadata") {
        None => None,
        Some(Value::Null) => {
            // Clearing the metadata is refused while the daemon owns a binding
            // in it: the binding is the daemon's record, not the user's.
            if truthy(existing_metadata.get("strategyBinding")) {
                return Err(PatchError::bad_request(
                    "metadata cannot be cleared while strategyBinding is daemon-owned",
                ));
            }
            if truthy(existing_metadata.get("exampleBinding")) {
                return Err(PatchError::bad_request(
                    "metadata cannot be cleared while exampleBinding is daemon-owned",
                ));
            }
            if truthy(existing_metadata.get("baseDir")) {
                return Err(PatchError::bad_request(
                    "metadata cannot be cleared for imported projects",
                ));
            }
            Some(Value::Null)
        }
        Some(Value::Object(document)) => Some(Value::Object(document.clone())),
        Some(_) => {
            return Err(PatchError::bad_request(
                "metadata must be an object or null",
            ))
        }
    };

    if let Some(Value::Object(document)) = metadata.as_mut() {
        // The daemon owns these bindings: a patch may repeat them, never
        // change them.
        if let Some(scopes) = document.get("localCatalogScopes") {
            if !same_json(Some(scopes), existing_metadata.get("localCatalogScopes")) {
                return Err(PatchError::bad_request(
                    "localCatalogScopes can only be set during project creation",
                ));
            }
        }
        for key in ["scenarioBinding", "strategyBinding", "exampleBinding"] {
            if let Some(value) = document.get(key) {
                if !same_json(Some(value), existing_metadata.get(key)) {
                    return Err(PatchError::bad_request(format!("{key} is daemon-owned")));
                }
            }
        }
        if let Some(value) = document.get("fromTrustedPicker") {
            if !same_json(Some(value), existing_metadata.get("fromTrustedPicker")) {
                return Err(PatchError::bad_request(
                    "fromTrustedPicker can only be set via POST /api/import/folder",
                ));
            }
        }
        if let Some(value) = document.get("orchestratorWorkspace") {
            parse_orchestrator_workspace(Some(value)).map_err(PatchError::bad_request)?;
        }

        let imported = truthy(existing_metadata.get("baseDir"));
        if imported {
            if let Some(value) = document.get("orchestratorWorkspace") {
                let existing_workspace = existing_metadata.get("orchestratorWorkspace");
                if !truthy(existing_workspace)
                    || !same_orchestrator_workspace(Some(value), existing_workspace)
                {
                    return Err(PatchError::bad_request(
                        "orchestratorWorkspace is immutable after import; use the working-dir route to change it",
                    ));
                }
            }
            if let Some(value) = document.get("baseDir") {
                if !same_json(Some(value), existing_metadata.get("baseDir")) {
                    return Err(PatchError::bad_request(
                        "baseDir is immutable after import; use a new import to change it",
                    ));
                }
            }
            // Re-stamp the import-owned fields: a metadata patch must never
            // detach the project from the folder it was imported from.
            if let Some(scopes) = existing_metadata.get("localCatalogScopes") {
                document.insert("localCatalogScopes".to_string(), scopes.clone());
            }
            if let Some(base_dir) = existing_metadata.get("baseDir") {
                document.insert("baseDir".to_string(), base_dir.clone());
            }
            for imported_from in ["folder", "project-location"] {
                if existing_metadata
                    .get("importedFrom")
                    .and_then(Value::as_str)
                    == Some(imported_from)
                {
                    document.insert(
                        "importedFrom".to_string(),
                        Value::String(imported_from.to_string()),
                    );
                }
            }
            if let Some(location) = existing_metadata
                .get("projectLocationId")
                .filter(|value| value.is_string())
            {
                document.insert("projectLocationId".to_string(), location.clone());
            }
            if existing_metadata.get("fromTrustedPicker") == Some(&Value::Bool(true)) {
                document.insert("fromTrustedPicker".to_string(), Value::Bool(true));
            }
            if let Some(workspace) = existing_metadata
                .get("orchestratorWorkspace")
                .filter(|value| truthy(Some(value)))
            {
                document.insert("orchestratorWorkspace".to_string(), workspace.clone());
            }
        } else if document.contains_key("baseDir") {
            return Err(PatchError::bad_request(
                "baseDir can only be set via POST /api/import/folder",
            ));
        } else if document.contains_key("orchestratorWorkspace") {
            return Err(PatchError::bad_request(
                "orchestratorWorkspace can only be set via POST /api/import/folder or POST /api/projects/:id/working-dir",
            ));
        } else if let Some(scopes) = existing_metadata.get("localCatalogScopes") {
            document.insert("localCatalogScopes".to_string(), scopes.clone());
        }

        // The daemon-owned bindings survive a patch that omits them.
        for key in ["scenarioBinding", "strategyBinding", "exampleBinding"] {
            if let Some(value) = existing_metadata
                .get(key)
                .filter(|value| truthy(Some(value)))
            {
                document.insert(key.to_string(), value.clone());
            }
        }

        if let Some(linked_dirs) = document
            .get("linkedDirs")
            .filter(|value| truthy(Some(value)))
        {
            let linked_dirs = linked_dirs.clone();
            let validated = validate_linked_dirs(&linked_dirs).map_err(|message| PatchError {
                code: "INVALID_LINKED_DIR".to_string(),
                message,
            })?;
            // A folder the desktop picker vouched for keeps its own spelling.
            if existing_metadata.get("fromTrustedPicker") != Some(&Value::Bool(true)) {
                document.insert(
                    "linkedDirs".to_string(),
                    Value::Array(validated.into_iter().map(Value::String).collect()),
                );
            }
        }
    }

    // ---- the catalogue-bound ids ----------------------------------------
    let skill_id = match patch.get("skillId") {
        None => existing.skill_id.clone(),
        Some(Value::Null) => None,
        Some(Value::String(id)) if id.is_empty() => None,
        Some(Value::String(id)) => {
            if !catalogue.skills.contains(id) {
                return Err(PatchError {
                    code: "SKILL_NOT_FOUND".to_string(),
                    message: "skill not found".to_string(),
                });
            }
            Some(id.clone())
        }
        Some(_) => {
            return Err(PatchError {
                code: "INVALID_SKILL_ID".to_string(),
                message: "skillId must be a string or null".to_string(),
            })
        }
    };
    let design_system_id = match patch.get("designSystemId") {
        None => existing.design_system_id.clone(),
        Some(Value::Null) => None,
        Some(Value::String(id)) if id.is_empty() => None,
        Some(Value::String(id)) => {
            let Some(status) = catalogue.design_systems.get(id) else {
                return Err(PatchError {
                    code: "DESIGN_SYSTEM_NOT_FOUND".to_string(),
                    message: "design system not found".to_string(),
                });
            };
            if status == "draft" {
                return Err(PatchError {
                    code: "DESIGN_SYSTEM_NOT_PUBLISHED".to_string(),
                    message: "draft design systems cannot be used by projects".to_string(),
                });
            }
            Some(id.clone())
        }
        Some(_) => {
            return Err(PatchError {
                code: "INVALID_DESIGN_SYSTEM".to_string(),
                message: "designSystemId must be a string or null".to_string(),
            })
        }
    };

    // ---- the free-form fields -------------------------------------------
    let name = match patch.get("name") {
        None => existing.name.clone(),
        Some(Value::String(name)) => name.clone(),
        Some(_) => return Err(PatchError::bad_request("name must be a string")),
    };
    let custom_instructions = match patch.get("customInstructions") {
        None => existing.custom_instructions.clone(),
        Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if text.chars().count() > 5000 {
                return Err(PatchError::bad_request(
                    "customInstructions exceeds 5 000 character limit",
                ));
            }
            Some(text.clone())
        }
        Some(_) => {
            return Err(PatchError::bad_request(
                "customInstructions must be a string or null",
            ))
        }
    };
    let pending_prompt = match patch.get("pendingPrompt") {
        None => existing.pending_prompt.clone(),
        Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(PatchError::bad_request(
                "pendingPrompt must be a string or null",
            ))
        }
    };

    // ---- the catalogue scopes -------------------------------------------
    // Rebinding the skill or the design system drops the catalogue scope it
    // pinned: the project no longer reads from that catalogue.
    let skill_changed = patch.contains_key("skillId") && skill_id != existing.skill_id;
    let design_system_changed =
        patch.contains_key("designSystemId") && design_system_id != existing.design_system_id;
    if skill_changed || design_system_changed {
        let current = match metadata.as_ref() {
            Some(Value::Object(document)) => Value::Object(document.clone()),
            _ => existing_metadata.clone(),
        };
        if let Value::Object(mut document) = current {
            if let Some(Value::Object(scopes)) = document.get("localCatalogScopes").cloned() {
                let mut next_scopes = scopes;
                if skill_changed {
                    next_scopes.remove("skill");
                }
                if design_system_changed {
                    next_scopes.remove("designSystem");
                }
                document.remove("localCatalogScopes");
                if !next_scopes.is_empty() {
                    document.insert("localCatalogScopes".to_string(), Value::Object(next_scopes));
                }
                metadata = Some(Value::Object(document));
            }
        }
    }

    let metadata_json = match metadata {
        Some(Value::Null) => None,
        Some(value) => Some(value.to_string()),
        None => existing.metadata_json.clone(),
    };
    let updated_at = match patch.get("updatedAt") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(now),
        Some(Value::String(text)) if text == SYNC_KEEPS_UPDATED_AT => existing.updated_at,
        _ => now,
    };

    Ok(ProjectUpdate {
        name,
        skill_id,
        design_system_id,
        pending_prompt,
        metadata_json,
        custom_instructions,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, metadata: Option<Value>) -> ProjectRow {
        ProjectRow {
            id: "p-1".to_string(),
            name: name.to_string(),
            skill_id: None,
            design_system_id: None,
            pending_prompt: None,
            metadata_json: metadata.map(|value| value.to_string()),
            custom_instructions: None,
            created_at: 10,
            updated_at: 10,
        }
    }

    fn catalogue() -> Catalogue {
        let mut catalogue = Catalogue::default();
        catalogue.skills.insert("deck".to_string());
        catalogue
            .design_systems
            .insert("brand".to_string(), "published".to_string());
        catalogue
    }

    #[test]
    fn a_name_patch_keeps_every_other_field() {
        let existing = row("Deck", Some(json!({ "kind": "prototype" })));
        let update = compose_project_patch(
            &existing,
            &json!({ "name": "  Deck 2  " }),
            &catalogue(),
            20,
        )
        .expect("valid");
        assert_eq!(update.name, "  Deck 2  ");
        assert_eq!(
            update.metadata_json,
            Some(json!({ "kind": "prototype" }).to_string())
        );
        assert_eq!(update.updated_at, 20);
    }

    #[test]
    fn the_daemon_owned_bindings_cannot_be_changed() {
        let existing = row("Deck", Some(json!({ "scenarioBinding": { "id": "s-1" } })));
        let error = compose_project_patch(
            &existing,
            &json!({ "metadata": { "scenarioBinding": { "id": "s-2" } } }),
            &catalogue(),
            20,
        )
        .expect_err("refused");
        assert_eq!(error.code, "BAD_REQUEST");
        assert_eq!(error.message, "scenarioBinding is daemon-owned");

        // Repeating the same binding is allowed.
        let update = compose_project_patch(
            &existing,
            &json!({ "metadata": { "scenarioBinding": { "id": "s-1" } } }),
            &catalogue(),
            20,
        )
        .expect("valid");
        assert_eq!(
            update.metadata_json,
            Some(json!({ "scenarioBinding": { "id": "s-1" } }).to_string())
        );
    }

    #[test]
    fn an_imported_project_keeps_its_folder() {
        let existing = row(
            "Deck",
            Some(json!({
                "baseDir": "/home/user/deck",
                "importedFrom": "folder",
                "entryFile": "index.html",
                "fromTrustedPicker": true,
            })),
        );
        // A patch that drops the folder silently is repaired by the re-stamp.
        let update = compose_project_patch(
            &existing,
            &json!({ "metadata": { "kind": "prototype" } }),
            &catalogue(),
            20,
        )
        .expect("valid");
        let metadata: Value =
            serde_json::from_str(&update.metadata_json.expect("metadata")).unwrap();
        assert_eq!(metadata["baseDir"], "/home/user/deck");
        assert_eq!(metadata["importedFrom"], "folder");
        assert_eq!(metadata["fromTrustedPicker"], true);

        // Moving the folder is refused.
        let error = compose_project_patch(
            &existing,
            &json!({ "metadata": { "baseDir": "/home/user/other" } }),
            &catalogue(),
            20,
        )
        .expect_err("refused");
        assert_eq!(
            error.message,
            "baseDir is immutable after import; use a new import to change it"
        );

        // Clearing the metadata of an imported project is refused.
        let error =
            compose_project_patch(&existing, &json!({ "metadata": null }), &catalogue(), 20)
                .expect_err("refused");
        assert_eq!(
            error.message,
            "metadata cannot be cleared for imported projects"
        );
    }

    #[test]
    fn a_plain_project_cannot_acquire_a_folder() {
        let existing = row("Deck", Some(json!({ "kind": "prototype" })));
        let error = compose_project_patch(
            &existing,
            &json!({ "metadata": { "baseDir": "/home/user/deck" } }),
            &catalogue(),
            20,
        )
        .expect_err("refused");
        assert_eq!(
            error.message,
            "baseDir can only be set via POST /api/import/folder"
        );
    }

    #[test]
    fn the_catalogue_ids_are_validated() {
        let existing = row("Deck", None);
        let error =
            compose_project_patch(&existing, &json!({ "skillId": "nope" }), &catalogue(), 20)
                .expect_err("refused");
        assert_eq!(error.code, "SKILL_NOT_FOUND");

        let error = compose_project_patch(
            &existing,
            &json!({ "designSystemId": "nope" }),
            &catalogue(),
            20,
        )
        .expect_err("refused");
        assert_eq!(error.code, "DESIGN_SYSTEM_NOT_FOUND");

        let update = compose_project_patch(
            &existing,
            &json!({ "skillId": "deck", "designSystemId": "brand" }),
            &catalogue(),
            20,
        )
        .expect("valid");
        assert_eq!(update.skill_id.as_deref(), Some("deck"));
        assert_eq!(update.design_system_id.as_deref(), Some("brand"));
    }

    #[test]
    fn the_custom_instructions_are_bounded() {
        let existing = row("Deck", None);
        let error = compose_project_patch(
            &existing,
            &json!({ "customInstructions": 7 }),
            &catalogue(),
            20,
        )
        .expect_err("refused");
        assert_eq!(error.message, "customInstructions must be a string or null");

        let long = "x".repeat(5001);
        let error = compose_project_patch(
            &existing,
            &json!({ "customInstructions": long }),
            &catalogue(),
            20,
        )
        .expect_err("refused");
        assert_eq!(
            error.message,
            "customInstructions exceeds 5 000 character limit"
        );
    }

    #[test]
    fn rebinding_the_skill_drops_its_catalogue_scope() {
        let existing = row(
            "Deck",
            Some(json!({ "localCatalogScopes": { "skill": "deck", "designSystem": "brand" } })),
        );
        // The project had no skill; binding one drops the skill scope and
        // keeps the design-system one.
        let update =
            compose_project_patch(&existing, &json!({ "skillId": "deck" }), &catalogue(), 20)
                .expect("valid");
        let metadata: Value =
            serde_json::from_str(&update.metadata_json.expect("metadata")).unwrap();
        assert_eq!(
            metadata["localCatalogScopes"],
            json!({ "designSystem": "brand" })
        );

        // A patch may not write the scopes at all.
        let error = compose_project_patch(
            &existing,
            &json!({ "metadata": { "localCatalogScopes": { "skill": "other" } } }),
            &catalogue(),
            20,
        )
        .expect_err("refused");
        assert_eq!(
            error.message,
            "localCatalogScopes can only be set during project creation"
        );
    }
}
