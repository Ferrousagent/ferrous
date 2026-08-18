# Risk Register: T1.1–T1.2 WASI and Native Runtime

This register records the Phase 1 execution-boundary risks that are now covered
by implementation and cloud CI tests. Native PTY support is enabled only on
Unix hosts in Phase 1; unsupported hosts fail closed until their platform
adapter is implemented and tested.

| ID | Risk | Mitigation | Verification |
| --- | --- | --- | --- |
| R8 | Native commands could inherit secrets from the host environment. | `selected_environment` queries and forwards only names explicitly allowlisted by the capability grant. | `native::tests::only_allowlisted_environment_reaches_the_child`; `policy::tests::unallowed_names_are_never_queried` |
| R11 | A native request could bypass policy or fall back to ambient execution. | Native mode requires `allow_native_execution` and broker approval; unsupported hosts return `UnsupportedOnHost`; no ambient fallback exists. | `native::tests::spawn_requires_explicit_grant`; `native::tests::supported_on_host_matches_the_platform_adapter`; `broker::tests::native_submission_requires_approval_then_streams_output`; `contract_tests::native_backend_never_falls_back_to_ambient_execution` |
| R23 | PTY/process cancellation could leave the session running. | The session driver owns reader, watchdog, and input threads; cancellation and timeouts kill the child before joining all threads. | `broker::tests::native_cancel_reports_cancelled_and_releases_the_session`; `native_session::tests::cancellation_kills_the_native_process_tree` |
| R34 | Shell metacharacters could become unintended commands. | Native requests use structured `CommandBuilder` argv directly; the CLI tokenizer only groups arguments and never executes a shell string. | `native::tests::spawn_uses_direct_argv_and_ignores_shell_metacharacters`; `shell::tests::run_native_preserves_quoted_metacharacters_as_one_argument` |
| R35 | A cancelled command could orphan descendants. | Unix PTY sessions are killed through the platform process-group boundary; teardown waits for the process and owned threads to finish. | `native_session::tests::cancellation_kills_the_native_process_tree` |

## Remaining platform scope

- Windows ConPTY and any future non-Unix native policy adapter remain
  fail-closed until they have equivalent process-tree and policy tests.
- The human-facing terminal renderer (wterm or another renderer) remains a
  later UI-phase adapter. The backend contract is renderer-independent.
