# Settings, providers and policy rules

The settings surface holds LLM/TTS/STT/Embedding/MCP/ACP connections, the
provider catalog, and the saved policy rules that decide whether a device
capability runs automatically, asks, or is refused.

## Sub-features

- `SettingsService/GetSettings` — settings revision and connection list.
- `SettingsService/UpdateSettings` — add/update connections with
  `expectedRevision` (stale revision is `aborted`).
- `SettingsService/ListProviders` — the static provider catalog (OpenAI,
  OpenRouter, Anthropic, Gemini) with endpoint and credential requirement.
- `SettingsService/ListModels` — models for a configured connection
  (`connectionId`); 404 without a connection.
- `SettingsService/ListPolicyRules` / `UpdatePolicyRules` — saved rules keyed by
  tool name, pattern and effect (`POLICY_EFFECT_ALLOW`, `POLICY_EFFECT_DENY`,
  `POLICY_EFFECT_CONFIRM`); `addOrUpdate` and `removeIds`.
- `SettingsService/ListPendingApprovals` — approval cards waiting for the user.

## How to get to it (user POV)

The user opens settings, adds an LLM connection, and the provider/model pickers
populate. From a chat approval card they choose "always allow" and the rule
appears under permissions. This is the first-run configuration path: without a
connection there is no model to assign to a companion.

## Driving it with connect.py

```sh
py=.agents/skills/verify-ene-host/scripts

$py/connect.py rpc ene.host.v1.SettingsService/GetSettings '{}'
$py/connect.py rpc ene.host.v1.SettingsService/ListProviders '{}'
$py/connect.py rpc ene.host.v1.SettingsService/ListPendingApprovals '{}'

# write a saved rule, then read it back through a different call
$py/connect.py rpc ene.host.v1.SettingsService/UpdatePolicyRules \
  '{"addOrUpdate":[{"id":"r1","toolName":"body_motion","pattern":"*","effect":"POLICY_EFFECT_ALLOW"}]}'
$py/connect.py rpc ene.host.v1.SettingsService/ListPolicyRules '{}'

# remove it and confirm the list is empty again
$py/connect.py rpc ene.host.v1.SettingsService/UpdatePolicyRules '{"removeIds":["r1"]}'
$py/connect.py rpc ene.host.v1.SettingsService/ListPolicyRules '{}'
```

Capture both policy responses, the settings revision, and `doctor` output.

## Observable end state

- `GetSettings` returns `revision` and `updatedAt` (connections absent when
  none exist — repeated fields are omitted, not `[]`).
- `ListProviders` returns four providers with `adapter`, `endpoint`,
  `requiresCredentials` and the environment variable name.
- `UpdatePolicyRules` returns the full rule list including the new rule with
  `createdAt`; `ListPolicyRules` then returns the same rule.
- After `removeIds`, `ListPolicyRules` returns `{}`.
- `ListPendingApprovals` returns `{}` on an idle host.

## Gotchas

- Enum values are proto names (`POLICY_EFFECT_ALLOW`, not `allow`);
  `"allow"` returns 400 `failed to decode JSON ... expected a known enum
  variant name`.
- `uint64` fields travel as JSON strings (`"revision":"1"`, `"revision":"0"`);
  numbers may decode, but compare as strings.
- Repeated/empty message fields are omitted from protobuf JSON; test
  `{}` rather than `{"items":[]}`.
- `ListModels` requests need a real `connectionId`; a fresh host has none, so
  it returns 404 `connection not found` — that is the precondition failing, not
  the endpoint.
- `UpdateSettings` writes connections but credentials live in the OS keyring
  (or memory with `ENE_HOST_SESSION_CREDENTIALS=1`); a connection without
  credentials reports them as missing, not invalid.
- Policy rules never bypass per-operation approval by themselves
  (`ENE_HOST_ALLOWED_CAPABILITIES` cannot be unset for a capability to be
  granted at all); see [clients.md](clients.md).
