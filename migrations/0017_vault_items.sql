CREATE TABLE vault_items (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    metadata TEXT NOT NULL,
    fields TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK(typeof(id)='text' AND length(id)=36 AND length(CAST(id AS BLOB))=36 AND id=lower(id) COLLATE BINARY AND substr(id,9,1)='-' AND substr(id,14,1)='-' AND substr(id,19,1)='-' AND substr(id,24,1)='-' AND length(replace(id,'-',''))=32 AND replace(id,'-','') NOT GLOB '*[^0-9a-f]*' AND id<>'00000000-0000-0000-0000-000000000000'),
    CHECK(kind IN ('login','api_secret','card','identity')),
    CHECK(state IN ('pending','ready','removing')),
    CHECK(json_valid(metadata) AND json_type(metadata)='object' AND json_extract(metadata,'$.kind')=kind AND length(CAST(metadata AS BLOB)) BETWEEN 2 AND 4096),
    CHECK(json_valid(fields) AND json_type(fields)='array' AND json_array_length(fields) BETWEEN 1 AND 3 AND length(CAST(fields AS BLOB))<=256),
    CHECK(typeof(created_at)='integer' AND created_at>0),
    CHECK(typeof(updated_at)='integer' AND updated_at>=created_at)
) STRICT;

CREATE INDEX vault_items_by_state ON vault_items(state, updated_at DESC, id);
