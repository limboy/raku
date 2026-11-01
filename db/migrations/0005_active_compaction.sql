ALTER TABLE `sessions` ADD `active_compaction` text;--> statement-breakpoint
-- One-time backfill for sessions saved before maintenance had a list column.
UPDATE sessions SET active_compaction = (
  SELECT CASE WHEN json_valid(data) THEN json_extract(data, '$.active_compaction') END FROM session_details
  WHERE session_id = sessions.id
);
