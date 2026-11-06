/**
 * Raku local state schema.
 *
 * Drizzle is a build-time tool here: `bun run db:generate` diffs this file and
 * writes plain SQL into `db/migrations`, which the Rust app applies at startup
 * (see `apply_migrations` in `src/persistence.rs`). drizzle-orm never ships in
 * the binary — Rust owns every query.
 *
 * Session history is kept out of the `sessions` row: the row holds only what
 * the session list renders, so listing is a scan over narrow rows. The
 * transcript lives in `session_details` and messages in `messages`, both
 * fetched only when a session is opened.
 */

import { index, integer, sqliteTable, text, uniqueIndex } from "drizzle-orm/sqlite-core";

export const projects = sqliteTable("projects", {
  id: text("id").primaryKey(),
  name: text("name").notNull(),
  path: text("path").notNull(),
  /** Order shown in the sidebar. */
  position: integer("position").notNull(),
  /** When the project was added, unix seconds. */
  createdAt: integer("created_at").notNull(),
});

export const sessions = sqliteTable(
  "sessions",
  {
    id: text("id").primaryKey(),
    projectId: text("project_id").notNull(),
    /** Explicit user title; "New task" means the automatic fallback is active. */
    title: text("title").notNull(),
    /** Provider-generated title, with the first prompt as a local fallback. */
    autoTitle: text("auto_title"),
    provider: text("provider").notNull(),
    model: text("model"),
    status: text("status").notNull(),
    /** Maintenance can be busy without a provider turn. Needed before hydration. */
    activeCompaction: text("active_compaction"),
    /** Side-chat origin JSON (parent task, quoted anchor); null for tasks. Needed before hydration. */
    sideChat: text("side_chat"),
    /** Session creation time, unix seconds. */
    createdAt: integer("created_at").notNull(),
    /** Any mutation, unix seconds — including title edits and truncation. */
    updatedAt: integer("updated_at").notNull(),
    /** Completion of the most recent assistant turn, unix seconds. */
    lastReplyAt: integer("last_reply_at"),
  },
  (table) => [
    index("sessions_by_project").on(table.projectId, table.updatedAt),
    index("sessions_by_updated_at").on(table.updatedAt),
    index("sessions_by_last_reply_at").on(table.lastReplyAt),
  ],
);

/**
 * Conversation messages, one row each.
 *
 * Split out of `sessions.data` so appending to a long conversation writes one
 * small row instead of rewriting the whole history, and so a message can be
 * read or counted without deserializing a transcript.
 */
export const messages = sqliteTable(
  "messages",
  {
    id: text("id").primaryKey(),
    sessionId: text("session_id").notNull(),
    turnId: text("turn_id"),
    /** Ordinal within the session; conversation order, not wall-clock. */
    position: integer("position").notNull(),
    role: text("role").notNull(),
    content: text("content").notNull(),
    /** User-visible text before provider-facing attachment mentions. */
    displayContent: text("display_content"),
    /** JSON-serialized MessageAttachment array. */
    attachments: text("attachments").notNull().default("[]"),
    createdAt: integer("created_at").notNull(),
    streaming: integer("streaming", { mode: "boolean" }).notNull(),
    /** JSON MessageError when the reply reports a failed turn; null otherwise. */
    error: text("error"),
  },
  (table) => [index("messages_by_session").on(table.sessionId, table.position)],
);

/**
 * The rest of `AgentSession` as JSON — transcript blocks, turns, provider
 * cursor.
 *
 * Split from `sessions` because it is large and rarely read: keeping it in the
 * row would mean listing sessions pages through every transcript, and every
 * title edit rewrites a transcript-sized row.
 */
export const sessionDetails = sqliteTable("session_details", {
  sessionId: text("session_id").primaryKey(),
  data: text("data").notNull(),
});

/**
 * Review contexts storing immutable Git snapshot and patch information.
 */
export const reviewContexts = sqliteTable("review_contexts", {
  id: text("id").primaryKey(),
  cwd: text("cwd").notNull(),
  /** JSON-serialized ReviewDiffSource */
  source: text("source").notNull(),
  baseOid: text("base_oid").notNull(),
  targetOid: text("target_oid").notNull(),
  numstat: text("numstat").notNull(),
  patch: text("patch").notNull(),
  completeContext: integer("complete_context", { mode: "boolean" }).notNull(),
  snapshotDir: text("snapshot_dir"),
  createdAt: integer("created_at").notNull(),
});

/**
 * Top-level groups of delegated subtasks spawned by a parent turn.
 */
export const delegationGroups = sqliteTable(
  "delegation_groups",
  {
    id: text("id").primaryKey(),
    parentSessionId: text("parent_session_id").notNull(),
    parentTurnId: text("parent_turn_id"),
    parentRuntimeId: text("parent_runtime_id"),
    requestKey: text("request_key").notNull(),
    requestHash: text("request_hash").notNull(),
    contextId: text("context_id").notNull(),
    status: text("status").notNull(),
    createdAt: integer("created_at").notNull(),
    updatedAt: integer("updated_at").notNull(),
  },
  (table) => [
    index("delegation_groups_by_parent").on(table.parentSessionId, table.createdAt),
    uniqueIndex("delegation_groups_by_request_key").on(table.parentSessionId, table.requestKey),
  ],
);

/**
 * Individual subtasks belonging to a delegation group.
 */
export const delegatedTasks = sqliteTable(
  "delegated_tasks",
  {
    id: text("id").primaryKey(),
    groupId: text("group_id").notNull(),
    taskKey: text("task_key").notNull(),
    childSessionId: text("child_session_id").notNull(),
    runtimeId: text("runtime_id"),
    /** JSON-serialized ModelReference */
    targetConfig: text("target_config").notNull(),
    instructions: text("instructions").notNull(),
    status: text("status").notNull(),
    finalConclusion: text("final_conclusion"),
    errorMessage: text("error_message"),
    latestActivity: text("latest_activity"),
    startedAt: integer("started_at"),
    finishedAt: integer("finished_at"),
  },
  (table) => [
    index("delegated_tasks_by_group").on(table.groupId),
    index("delegated_tasks_by_child").on(table.childSessionId),
  ],
);

/**
 * Ordered stream events emitted by child runtimes for replay and persistence.
 */
export const delegationEvents = sqliteTable(
  "delegation_events",
  {
    id: integer("id").primaryKey({ autoIncrement: true }),
    taskId: text("task_id").notNull(),
    sequence: integer("sequence").notNull(),
    event: text("event").notNull(),
    createdAt: integer("created_at").notNull(),
  },
  (table) => [
    index("delegation_events_by_task").on(table.taskId, table.id),
  ],
);
