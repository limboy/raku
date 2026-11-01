DROP INDEX `delegation_events_by_task`;--> statement-breakpoint
CREATE INDEX `delegation_events_by_task` ON `delegation_events` (`task_id`,`id`);