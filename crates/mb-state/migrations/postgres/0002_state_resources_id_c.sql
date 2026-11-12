-- The ids of one type of a session in byte order ("C"), whatever the database's
-- collation: `list_prefix` reads the ids under a prefix (a parent's) as a range of
-- this index, `id COLLATE "C" >= prefix AND id COLLATE "C" < next`.
CREATE INDEX state_resources_id_c
    ON state_resources (workspace, mock, session, resource_type, id COLLATE "C");
