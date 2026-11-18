# ene-host feature map

One file per user-facing capability of the host API. Every journey is written
from the user's point of view, drives the same RPC the desktop client drives,
and states an observable end condition. When a journey is blocked by a missing
precondition, it fails and the reason goes in the file's `Gotchas`; never
weaken the journey to make it pass.

| Feature | File | Drivable today |
| --- | --- | --- |
| Discovery, auth and process lifecycle | [connection-and-auth.md](connection-and-auth.md) | yes |
| Companions and their mind profile | [companions.md](companions.md) | yes (a model assignment is not required to create one) |
| Notifications: subscribe, cursor, live events | [notifications.md](notifications.md) | yes |
| Settings, providers and policy rules | [settings-and-policy.md](settings-and-policy.md) | yes |
| Client registration and device stream | [clients.md](clients.md) | yes |

Driving conventions (launch, doctor, evidence, cleanup) live in
[../SKILL.md](../SKILL.md).
