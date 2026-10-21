# ADR-0015: Polled non-durable live assistant text

Status: Accepted. Hosted provider streaming through the Host is unverified.

## Context

The OpenAI and Anthropic adapters already decode provider SSE incrementally and the native loop yields provisional text deltas, but the headless Host consumed them inside the scheduler worker. Attached clients saw nothing until the committed assistant message, so a long answer appeared in one burst. Owner IPC is one request and one response per connection with bounded long-poll waits, the Mac app reaches the Host only through one-shot `owner --request-file` CLI children, and the durable event journal, checkpoints and encrypted synchronization batches are the authoritative history.

## Decision

The scheduler keeps one in-memory `LiveText { attempt_id, text }` entry per running task, replaced at each model attempt, appended on each provisional delta up to the chat text limit and removed when the worker returns. The `task` owner query returns it as an optional additive `execution.live_text` field. The worker's existing change hint wakes `wait`, so clients poll it with requests they already make. Text deltas are not appended as conversation events, are not checkpointed and are not synchronized to the relay.

## Consequences

Writers never wait for readers, so slow, stalled or disconnected clients cannot affect the provider turn, and no per-client buffer exists to bound. Clients may skip intermediate snapshots and must replace live text with the committed message. A restart loses live text and replay is unchanged. Polling repeats the accumulated text in each response, bounded by the 128 KiB chat limit inside the 2 MiB frame. Paired devices see only committed messages until a separate encrypted live channel is designed. Older clients ignore the field and newer clients see an older Host as never streaming.
