CREATE TABLE `documents` (
	`id` text PRIMARY KEY NOT NULL,
	`owner` text NOT NULL,
	`title` text NOT NULL,
	`kind` text NOT NULL,
	`source_url` text,
	`original_name` text NOT NULL,
	`mime` text NOT NULL,
	`status` text NOT NULL,
	`engine` text DEFAULT '' NOT NULL,
	`sha256` text NOT NULL,
	`bytes` integer NOT NULL,
	`created_at` text NOT NULL,
	`search_text` text DEFAULT '' NOT NULL
);
--> statement-breakpoint
CREATE INDEX `documents_owner_created` ON `documents` (`owner`,`created_at`);