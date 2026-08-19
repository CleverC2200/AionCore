-- Keep the stable internal agent identity while updating the user-facing
-- product name for both existing and fresh databases.
UPDATE agent_metadata
SET name = 'GEA CLI',
    updated_at = unixepoch('now','subsec') * 1000
WHERE id = '632f31d2'
  AND agent_type = 'aionrs'
  AND agent_source = 'internal'
  AND name = 'Aion CLI';
