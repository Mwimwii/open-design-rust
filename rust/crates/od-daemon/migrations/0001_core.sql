CREATE TABLE IF NOT EXISTS projects (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      skill_id TEXT,
      design_system_id TEXT,
      pending_prompt TEXT,
      metadata_json TEXT,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );

    -- A project belongs to exactly ONE workspace, so project_id is the key.
    -- See collab/workspace-project-home.ts for the ruling and the repair path.
    CREATE TABLE IF NOT EXISTS workspace_projects (
      project_id TEXT PRIMARY KEY,
      workspace_id TEXT NOT NULL,
      visibility TEXT NOT NULL CHECK (visibility IN ('personal', 'team')),
      resource_state TEXT NOT NULL CHECK (resource_state IN ('active', 'frozen', 'deleted')),
      created_by_workspace_member_id TEXT,
      updated_by_workspace_member_id TEXT,
      resource_hub_resource_id TEXT,
      cloud_tombstoned_at INTEGER,
      sync_state TEXT,
      metadata_refresh_pending INTEGER NOT NULL DEFAULT 0,
      version INTEGER NOT NULL DEFAULT 1,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_workspace_projects_workspace_visibility
      ON workspace_projects(workspace_id, visibility, updated_at DESC);

    CREATE TABLE IF NOT EXISTS team_project_materializations (
      workspace_id TEXT NOT NULL,
      resource_team_id TEXT NOT NULL,
      viewer_member_id TEXT NOT NULL,
      owner_member_id TEXT NOT NULL,
      project_id TEXT NOT NULL,
      resource_id TEXT NOT NULL,
      ref TEXT NOT NULL CHECK (ref = 'published'),
      version INTEGER NOT NULL,
      version_id TEXT NOT NULL,
      manifest_digest TEXT NOT NULL,
      lifecycle_state TEXT NOT NULL CHECK (lifecycle_state = 'active'),
      authorized_at TEXT NOT NULL,
      expires_at TEXT NOT NULL,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (workspace_id, project_id),
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    -- The generic workspace-binding table for resource types that do NOT get
    -- their own dedicated table (plugin today; skill / design system are
    -- planned follow-ups — see specs/current for the phased rollout). Same
    -- "binding envelope" columns as workspace_projects, parameterized by
    -- resource_type so one CRUD layer (see getWorkspaceResource and friends
    -- below) and one mutation gate (collab/workspace-resource-mutation.ts)
    -- serve every resource type instead of forking per type.
    --
    -- Unlike workspace_projects, resource_id has no FOREIGN KEY here: which
    -- table it points at depends on resource_type, and SQLite has no
    -- polymorphic foreign key. Callers that delete a resource's underlying
    -- record MUST also delete its workspace_resources row (by resource_type +
    -- resource_id) themselves, or it becomes an orphan binding — the same
    -- failure mode workspace_projects_legacy_single_project once hit.
    CREATE TABLE IF NOT EXISTS workspace_resources (
      resource_type TEXT NOT NULL,
      resource_id TEXT NOT NULL,
      workspace_id TEXT NOT NULL,
      visibility TEXT NOT NULL CHECK (visibility IN ('personal', 'team')),
      resource_state TEXT,
      created_by_workspace_member_id TEXT,
      updated_by_workspace_member_id TEXT,
      resource_hub_resource_id TEXT,
      cloud_tombstoned_at INTEGER,
      sync_state TEXT,
      version INTEGER,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      PRIMARY KEY (resource_type, resource_id)
    );

    CREATE INDEX IF NOT EXISTS idx_workspace_resources_type_workspace
      ON workspace_resources(resource_type, workspace_id, updated_at DESC);

    CREATE TABLE IF NOT EXISTS templates (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      description TEXT,
      source_project_id TEXT,
      files_json TEXT NOT NULL,
      created_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS conversations (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      title TEXT,
      session_mode TEXT NOT NULL DEFAULT 'design',
      intent_signals_json TEXT,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_conv_project
      ON conversations(project_id, updated_at DESC);

    CREATE TABLE IF NOT EXISTS agent_sessions (
      conversation_id TEXT NOT NULL,
      agent_id        TEXT NOT NULL,
      session_id      TEXT NOT NULL,
      stable_prompt_hash TEXT,
      -- Per-section digests of the stable prefix inputs behind
      -- stable_prompt_hash, as JSON (see prompts/stable-sections.ts). Purely
      -- diagnostic: when the hash moves, diffing this against the current turn
      -- names WHICH input drifted. Never gates a re-send -- stable_prompt_hash
      -- stays the only source of truth for that.
      stable_prompt_sections TEXT,
      -- Resume identity guard: the session is only safe to resume when the
      -- conversation has not changed shape under it. model/cwd are the runtime
      -- identity the upstream session was created with; a change forces a fresh
      -- session. last_message_id is the assistant message this session produced
      -- on its last turn -- if it is no longer the latest completed assistant
      -- turn (another agent ran in between, or it was edited away), the session
      -- is behind and we reseed the full transcript.
      model           TEXT,
      cwd             TEXT,
      last_message_id TEXT,
      -- Last provider-reported effective input usage for this exact session.
      -- Observability only: never used to admit, reject, compact, or roll over.
      last_input_tokens INTEGER,
      updated_at      INTEGER NOT NULL,
      PRIMARY KEY (conversation_id, agent_id),
      FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS messages (
      id TEXT PRIMARY KEY,
      conversation_id TEXT NOT NULL,
      role TEXT NOT NULL,
      content TEXT NOT NULL,
      agent_id TEXT,
      agent_name TEXT,
      result_delivery_state TEXT,
      events_json TEXT,
      attachments_json TEXT,
      produced_files_json TEXT,
      trace_object_files_json TEXT,
      feedback_json TEXT,
      pre_turn_file_names_json TEXT,
      session_mode TEXT,
      run_context_json TEXT,
      task_analytics_json TEXT,
      applied_plugin_snapshot_json TEXT,
      telemetry_finalized_at INTEGER,
      started_at INTEGER,
      ended_at INTEGER,
      position INTEGER NOT NULL,
      created_at INTEGER NOT NULL,
      FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_messages_conv
      ON messages(conversation_id, position);

    -- Agent streams write small immutable batches while a run is active. The
    -- batches are folded into messages.events_json once, at the terminal
    -- boundary, so a long thinking stream never rewrites its full history on
    -- every flush window.
    CREATE TABLE IF NOT EXISTS message_event_batches (
      id INTEGER PRIMARY KEY,
      message_id TEXT NOT NULL,
      events_json TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_message_event_batches_message
      ON message_event_batches(message_id, id);

    -- One row per one-time data maintenance pass that has run to completion
    -- (e.g. the heal of run events stored before the payload budget existed),
    -- so a finished pass is not re-scanned on every daemon start.
    CREATE TABLE IF NOT EXISTS daemon_maintenance_passes (
      name TEXT PRIMARY KEY,
      completed_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS preview_comments (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      conversation_id TEXT NOT NULL,
      file_path TEXT NOT NULL,
      element_id TEXT NOT NULL,
      selector TEXT NOT NULL,
      label TEXT NOT NULL,
      text TEXT NOT NULL,
      position_json TEXT NOT NULL,
      html_hint TEXT NOT NULL,
      selection_kind TEXT,
      member_count INTEGER,
      pod_members_json TEXT,
      style_json TEXT,
      attachments_json TEXT,
      slide_index INTEGER,
      slide_key INTEGER NOT NULL DEFAULT -1,
      note TEXT NOT NULL,
      status TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      anchor_state TEXT,
      anchored_version INTEGER,
      author_member_id TEXT,
      last_good_position_json TEXT,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
      FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_preview_comments_conversation
      ON preview_comments(project_id, conversation_id, updated_at DESC);
    CREATE INDEX IF NOT EXISTS idx_preview_comments_conversation_created
      ON preview_comments(project_id, conversation_id, created_at ASC);

    CREATE TABLE IF NOT EXISTS tabs (
      project_id TEXT NOT NULL,
      name TEXT NOT NULL,
      position INTEGER NOT NULL,
      is_active INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY(project_id, name),
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS tabs_state (
      project_id TEXT PRIMARY KEY,
      updated_at INTEGER NOT NULL,
      state_json TEXT,
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_tabs_project
      ON tabs(project_id, position);

    CREATE TABLE IF NOT EXISTS deployments (
      id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      file_name TEXT NOT NULL,
      provider_id TEXT NOT NULL,
      url TEXT NOT NULL,
      deployment_id TEXT,
      deployment_count INTEGER NOT NULL DEFAULT 1,
      target TEXT NOT NULL DEFAULT 'preview',
      status TEXT NOT NULL DEFAULT 'ready',
      status_message TEXT,
      reachable_at INTEGER,
      provider_metadata_json TEXT,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL,
      UNIQUE(project_id, file_name, provider_id),
      FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_deployments_project
      ON deployments(project_id, updated_at DESC);

    CREATE TABLE IF NOT EXISTS routines (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      prompt TEXT NOT NULL,
      schedule_kind TEXT NOT NULL,
      schedule_value TEXT NOT NULL,
      schedule_json TEXT,
      project_mode TEXT NOT NULL,
      project_id TEXT,
      skill_id TEXT,
      agent_id TEXT,
      context_json TEXT,
      enabled INTEGER NOT NULL DEFAULT 1,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS routine_runs (
      id TEXT PRIMARY KEY,
      routine_id TEXT NOT NULL,
      trigger TEXT NOT NULL,
      status TEXT NOT NULL,
      project_id TEXT NOT NULL,
      conversation_id TEXT NOT NULL,
      agent_run_id TEXT NOT NULL,
      started_at INTEGER NOT NULL,
      completed_at INTEGER,
      summary TEXT,
      error TEXT,
      error_code TEXT,
      FOREIGN KEY(routine_id) REFERENCES routines(id) ON DELETE CASCADE
    );

    CREATE TABLE IF NOT EXISTS routine_schedule_claims (
      routine_id TEXT NOT NULL,
      slot_at INTEGER NOT NULL,
      claimed_at INTEGER NOT NULL,
      PRIMARY KEY(routine_id, slot_at),
      FOREIGN KEY(routine_id) REFERENCES routines(id) ON DELETE CASCADE
    );

    CREATE INDEX IF NOT EXISTS idx_routine_runs_routine
      ON routine_runs(routine_id, started_at DESC);
