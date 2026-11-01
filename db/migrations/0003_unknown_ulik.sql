CREATE TABLE `delegated_tasks` (
	`id` text PRIMARY KEY NOT NULL,
	`group_id` text NOT NULL,
	`task_key` text NOT NULL,
	`child_session_id` text NOT NULL,
	`runtime_id` text,
	`target_config` text NOT NULL,
	`instructions` text NOT NULL,
	`status` text NOT NULL,
	`final_conclusion` text,
	`error_message` text,
	`latest_activity` text,
	`started_at` integer,
	`finished_at` integer
);
--> statement-breakpoint
CREATE INDEX `delegated_tasks_by_group` ON `delegated_tasks` (`group_id`);--> statement-breakpoint
CREATE INDEX `delegated_tasks_by_child` ON `delegated_tasks` (`child_session_id`);--> statement-breakpoint
CREATE TABLE `delegation_events` (
	`id` integer PRIMARY KEY AUTOINCREMENT NOT NULL,
	`task_id` text NOT NULL,
	`sequence` integer NOT NULL,
	`event` text NOT NULL,
	`created_at` integer NOT NULL
);
--> statement-breakpoint
CREATE INDEX `delegation_events_by_task` ON `delegation_events` (`task_id`,`sequence`);--> statement-breakpoint
CREATE TABLE `delegation_groups` (
	`id` text PRIMARY KEY NOT NULL,
	`parent_session_id` text NOT NULL,
	`parent_turn_id` text,
	`parent_runtime_id` text,
	`request_key` text NOT NULL,
	`request_hash` text NOT NULL,
	`context_id` text NOT NULL,
	`status` text NOT NULL,
	`created_at` integer NOT NULL,
	`updated_at` integer NOT NULL
);
--> statement-breakpoint
CREATE INDEX `delegation_groups_by_parent` ON `delegation_groups` (`parent_session_id`,`created_at`);--> statement-breakpoint
CREATE UNIQUE INDEX `delegation_groups_by_request_key` ON `delegation_groups` (`parent_session_id`,`request_key`);--> statement-breakpoint
CREATE TABLE `review_contexts` (
	`id` text PRIMARY KEY NOT NULL,
	`cwd` text NOT NULL,
	`source` text NOT NULL,
	`base_oid` text NOT NULL,
	`target_oid` text NOT NULL,
	`numstat` text NOT NULL,
	`patch` text NOT NULL,
	`complete_context` integer NOT NULL,
	`snapshot_dir` text,
	`created_at` integer NOT NULL
);
