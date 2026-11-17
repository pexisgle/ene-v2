# Journeys

End-to-end acceptance runs against a real `ene-host` binary and real providers.
Each script starts its own isolated instance, drives the API the way a client
would, checks the result through a different call, and leaves its artifacts and
logs under `target/verify-ene-host/`. They are not unit tests: run them after a
change that could alter observable host behaviour, and read the log when one
fails.

Prerequisites:

- `.env` with `OPENROUTER_API_KEY`.
  Load them with `set -a; . ./.env; set +a` before a run.
- `direnv` so the dev shell provides `python3` for `scripts/connect.py`.
- Network access to OpenRouter and OpenAI.
- `bubblewrap`, which `acp-sandbox.sh` fetches from nixpkgs.

Run one, or all of them:

```sh
set -a; . ./.env; set +a
direnv exec . bash .agents/skills/verify-ene-host/journeys/all.sh
direnv exec . bash .agents/skills/verify-ene-host/journeys/chat.sh
```

`all.sh` runs every journey in sequence and prints one PASS or FAIL line each;
a failing journey's log is at `target/verify-ene-host/<journey>.log`.

| Journey | Proves |
| --- | --- |
| `rpcs.sh` | The RPCs the feature pages do not drive: settings, providers, models, connection check, credentials, policy, companions, mind profile, memories, message reads, stop, client registration, capabilities, activation, perception. 31 checks. |
| `work.sh` | A work run asks a question and finishes after the answer, `SendWork` starts a follow-up, `StopWork` and `CloseWork` answer, a confirm-gated operation goes through `SubmitApproval` to the device and completes, memory is stored through `mind_update` and forgotten. 17 checks. |
| `chat.sh` | One conversation turn through the real provider reaches the history. |
| `schedule.sh` | A due schedule records its firing and starts no turn when the model cannot be bound, and the client is told. |
| `device.sh` | A registered and activated client takes the model's `body_motion` over `DeviceStream` and reports the result. |
| `mcp.sh` | A stdio MCP server (`mcp-server.py`) becomes a tool the model calls, and its output returns to the reply. |
| `acp-sandbox.sh` | A `network: isolated` ACP child has no direct route, reaches the network only through the host's proxy, and the proxy refuses anything but CONNECT. |
| `restart.sh` | A companion created without a model, and one whose connection is later disabled, do not stop the next start. |

Speech journeys are not here: `VoiceService` answers only through an installed
Wasm provider plugin, and no plugin in this repo serves TTS or STT. The
transport and the microphone binding are covered by
`apps/ene-host/tests/process.rs`.
