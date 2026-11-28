---
name: verify-ene-host
description: Verify the real ene-host against its authenticated Connect API using isolated scratch state.
---

# Verify ene-host

Use `crates/ene-rpc/proto/host.proto` for method names and request fields. The API prefix is
`ene.host.v2` and the host binds a private loopback address for each run.

```bash
host=.agents/skills/verify-ene-host/scripts/host.sh
connect=.agents/skills/verify-ene-host/scripts/connect.py
run=$("$host" start smoke)
export ENE_HOST_CONNECTION="$run/runtime/connection.json"
"$host" doctor smoke
python3 "$connect" rpc ene.host.v2.HostService/GetSnapshot '{}'
python3 "$connect" rpc ene.host.v2.ChatService/GetHistory '{"companionId":"ene","limit":10}'
"$host" restart smoke
"$host" stop smoke
```

Use the Rust integration tests in `apps/ene-host/src/verification.rs` for provider, approval,
device and persistence exercises. Do not copy the connection file or write its bearer token
to logs. Use `VERIFY_RUN_DIR` only for scratch directories dedicated to these checks.
