# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## Protected model and approval-review settings

`model_settings_policy = "locked"` keeps the invocation's effective model execution settings
fixed for addressable work. The protected state includes the model, configured provider definition
and endpoint, ChatGPT authentication endpoint and configured proxy-mode setting, nullable reasoning
effort, review model, plan-mode reasoning effort, the lock itself, and `server_model_validation`.
Operating-system and environment transport, proxy, and certificate-trust state are outside this
policy. Runtime settings updates, externally created or resumed threads, forks, and ordinary
subagents must retain that state. On a cold resume, the current invocation is authoritative: an
older persisted model/effort anchor is preserved as history and a current `ThreadSettingsApplied`
anchor is appended before new model work.

The TUI does not use an implicit app-server daemon while either policy is locked, because the
daemon connection has no capability or launch-authority handshake and may refer to an older or
differently configured process. Explicit remote app-server sessions are rejected while either
policy is locked because the remote protocol does not transport and verify the protected-settings
contract.

Locked model settings require an explicit, non-empty `model`. Codex refuses to start rather than
locking an unresolved catalog default, because a refreshed catalog could otherwise select a
different default for a later thread in the same invocation.

Locked model settings also disable provider fallback and remove safety-buffering retry choices that
would select a faster model. Codex keeps waiting on the original request instead of exposing a
model-changing action to the TUI or accepting a forged retry event.

Realtime voice is fixed-purpose OpenAI-selected harness machinery, not a substantive assistant or
delegated agent. Its model, endpoints, protocol and session mode, transport, voice, audio devices,
and prompt and context remain independently selected by the harness or configuration. When
realtime delegates work, that handoff enters the ordinary addressable-work path and therefore
retains the substantive model, provider, and reasoning lock.

`approvals_reviewer_policy = "locked"` independently keeps `approvals_reviewer` fixed. This lets an
invocation require automated approval review without coupling the reviewer's internal model choice
to the substantive-work model lock. Structurally internal fixed-purpose machinery—such as Guardian
approval review, compaction, memory extraction, title generation, and background maintenance—may
use model settings selected by the harness. Assistant and tool turns, Plan, `/review`, and ordinary
addressable subagents remain substantive work and may not change the locked state.

Both policies default to `"mutable"`. When a lock is explicitly requested in SessionFlags, the
effective protected state at that exact layer becomes a fail-closed launch contract. Later
SessionFlags, legacy managed config, MDM, and requirements keep their normal authority and
precedence, but Codex refuses to start if they make the requested locked state incompatible rather
than silently launching with different settings.

`server_model_validation = "warn"` preserves compatibility behavior: responses stream without a
required identity, and Codex warns when a reported identity is invalid or does not match the
request. `"require_match"` buffers each inference response until completion and releases it only if
the server reported the requested model without an invalid or conflicting identity. This validates
model identity only; the inference protocol does not currently report reasoning effort for
equivalent validation. This policy is independent of model locking: `"require_match"` by itself
preserves mutable safety-retry choices, while `model_settings_policy = "locked"` removes them.
Strict buffering fails before emitting output if a response exceeds 262,144 events or 64 MiB of
serialized event data.
