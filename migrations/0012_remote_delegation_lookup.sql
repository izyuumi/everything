CREATE INDEX tasks_remote_delegation ON tasks(json_extract(input,'$.remote.binding.delegation_id') COLLATE NOCASE)
WHERE json_extract(input,'$.scheduler_version')=1 AND json_type(input,'$.remote')='object';
