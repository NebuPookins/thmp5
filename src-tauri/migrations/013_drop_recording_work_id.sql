-- Remove the `work_id` foreign-key column from `recording`.  The `work` table was
-- dropped in migration 010, but the FK constraint remained, causing every
-- subsequent INSERT into `recording` to fail when foreign_keys=ON:
--   (code: 1) no such table: main.work

ALTER TABLE recording DROP COLUMN work_id;
