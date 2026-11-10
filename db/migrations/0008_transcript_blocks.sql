CREATE TABLE `transcript_blocks` (
	`session_id` text NOT NULL,
	`position` integer NOT NULL,
	`data` text NOT NULL,
	PRIMARY KEY(`session_id`, `position`)
);
