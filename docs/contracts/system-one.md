# System One selection contract

`SystemOneSelector::new(ToolSelector, SelectionConfig)` wraps an immutable offline selector. `from_toml` turns a sanitized configuration error into explicit local-fallback diagnostics. `with_credential_store` shares a host-owned native store without replacing its real backend. Call synchronous `prepare(&ExecutionAuthority, query, limit)` on a blocking worker. Queries, result limits and final selection budgets remain those of the [offline selector](selector.md).

`PreparedSelection` contains either a local `ToolSelection` or a `SystemOneDisclosure`. The disclosure exposes the provider, the exact POST destination, model, JSON payload, authority and SHA256 fingerprint. The host `SelectionDisclosure` carries the same `provider` field; it defaults to `typesafe` when absent. `approve(Duration)` consumes it and produces a non-cloneable, non-deserializable `ApprovedSelection`, valid for at most five minutes of monotonic time. Only a trusted owner interaction may call this method. Neither configuration parsing, model output, remote commands nor tool arguments constitute approval. The host passes its current authority to `select_approved`; a different selector, task, conversation, workspace, mode or optional allowlist invalidates the approval.

`SelectionConfig::system_one(provider, endpoint, model, reference)` validates the endpoint as below and then the provider: `openrouter` requires exactly `https://openrouter.ai/api/v1` (a trailing slash is tolerated), `local_model` requires a literal-loopback IP host, and every chat-completions provider takes a base URL, not the completions URL, so a path ending in `/chat/completions` is refused; violations return `ProviderEndpoint`, surfaced by Settings as "System One provider endpoint". Provider values on the wire are `typesafe`, `openrouter`, `openai_compatible` and `local_model`.

For `typesafe` the HTTP POST uses the configured full endpoint, including ordinary query parameters, and this body shape:

```json
{
  "model": "user-selected-model",
  "state": "approved task summary",
  "questions": {
    "tool": {
      "type": "choice",
      "instructions": "Which tool is most relevant to the task? Rank only by relevance; execution requires independent permission.",
      "criteria": {
        "none": "No listed tool is relevant",
        "tool_000": "tool_name: approved tool description"
      }
    }
  }
}
```

For `openrouter`, `openai_compatible` and `local_model` the POST goes to the base URL's path plus `/chat/completions`, keeping its query. The body carries the same task summary and candidate descriptions:

```json
{
  "model": "user-selected-model",
  "messages": [
    {"role": "system", "content": "Which tool is most relevant to the task? Rank only by relevance; execution requires independent permission. The user message is JSON with the task and the options keyed by option ID. Answer with only a JSON object {\"choice\": ID} where ID is exactly one option key, or \"none\" if no option is relevant."},
    {"role": "user", "content": "{\"options\":{\"none\":\"No listed tool is relevant\",\"tool_000\":\"tool_name: approved tool description\"},\"task\":\"approved task summary\"}"}
  ],
  "temperature": 0,
  "response_format": {
    "type": "json_schema",
    "json_schema": {
      "name": "tool_choice",
      "strict": true,
      "schema": {"type": "object", "additionalProperties": false, "required": ["choice"], "properties": {"choice": {"type": "string", "enum": ["none", "tool_000"]}}}
    }
  }
}
```

The response must be a chat completion with exactly one `choices` entry whose `message.content`, after trimming whitespace, is exactly a JSON object `{"choice": ID}` with no other fields. It is then validated as a choice-only answer below. Servers or models that reject `response_format` or `temperature` return an HTTP error and fall back locally. Reasoning models whose response, including any reasoning fields, exceeds 64 KiB, or whose answer takes longer than ten seconds, also fall back locally.

Candidate IDs are request-local positions, never executable names supplied by the service. There are at most 254 candidates plus `none`. Denied tools are absent; approval-requiring tools retain their permission metadata and disclosure warning. Locally unmatched tools may be considered remotely, with local ranking deciding which candidates fit the bound.

The response requires exactly one `answers.tool` answer with `type = "choice"` and a `choice` equal to an offered ID. Optional `confidence` must be finite and within 0–1. Optional `probabilities` must contain every offered ID exactly as normalized map keys, no unknown IDs, finite 0–1 values, and a total within 0.02 of one. The chosen ID must have maximal probability, and supplied confidence must match its probability within 0.02. A probability response ranks tools above the `none` probability; a minimal choice-only answer returns its chosen tool or no tools for `none`. Unknown additional service metadata is ignored; malformed required fields or invalid decisions trigger local fallback.

Successful classification has `method = "system_one"`. Failure fallback has `method = "local_fallback"` and a static sanitized reason. The original offline API retains `method = "local"`. Every result retains the `search_tools` definition, obeys the existing 512-KiB output budget, and preserves whole schemas. Results are advisory native model definitions; they are not Tool Broker grants.

No API automatically executes a pending disclosure. A declined approval stays offline: the host uses `ToolSelector::select_local`. Expired approvals, changed authority/catalog, changed visibility, missing configured credentials, native-store failure, timeout, HTTP failure, oversized response and invalid choice all keep local discovery available. Busy classification returns explicit local-fallback diagnostics and only the bounded `search_tools` definition, without another worker or catalog scan. Every admitted blocking worker retains its classification slot across caller cancellation until it actually finishes. Permission failures and cancellation remain explicit errors.
