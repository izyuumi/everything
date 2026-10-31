ALTER TABLE conversations ADD COLUMN agent_id TEXT;
ALTER TABLE tasks ADD COLUMN agent_id TEXT;
CREATE INDEX conversations_agent_id ON conversations(agent_id);
CREATE INDEX tasks_agent_id ON tasks(agent_id);
