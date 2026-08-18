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
| R36 | A session ID could be reused while the first session is still live, detaching its cancellation/input ownership. | IDs are reserved under the same lock as capacity accounting; `enqueue` rejects duplicates with `DuplicateSession` instead of overwriting. | `broker::tests::concurrent_duplicate_id_submissions_allow_exactly_one_winner`; `broker::tests::duplicate_id_is_rejected_without_replacing_the_first_session` |
| R37 | Approve/deny/cancel racing on the same parked session could emit two terminal outcomes or a ghost run. | Only the `pending`-map owner may report a terminal for a parked session; approve checks the cancel flag before dispatch and cancel falls through to the handle when the job is already dispatched. | `broker::tests::approve_cancel_race_yields_exactly_one_terminal_outcome` |
| R38 | A WASI guest splitting output across stdout+stderr could push up to twice the declared budget into the event stream. | `cap_to_remaining` caps every drained chunk to the remaining *combined* budget before emission; both pipes hard-close the moment the budget is exhausted. | `pipe::tests::cap_to_remaining_never_allows_over_budget_emission`; `command::tests::fuzzed_event_sequences_never_break_state_machine_invariants` |
| R39 | An unallowlisted host secret could leak into a native child. | A real-child sentinel test places a secret in the parent environment and proves it is absent from the child unless explicitly granted. | `native::tests::unallowlisted_sentinel_secret_never_reaches_the_child` |

## Adversarial test evidence

Run [`32152299862`](https://github.com/Ferrousagent/ferrous/actions/runs/32152299862)
passed on Ubuntu, macOS, and Windows in parallel: formatting, Clippy with
warnings denied, all workspace tests (318 passed), docs, benchmark
compilation, the Criterion hot-path job, and license/advisory checks. The
cargo cache reported a full match on every runner. The adversarial suite
includes a 30-iteration approve/cancel race hammer, an 8-thread duplicate-ID
flood behind a barrier, a 20,000-step deterministic PRNG state-machine fuzz,
and a real-child sentinel-secret leak proof.

## Performance evidence

The cached GitHub Actions performance job runs:

```text
cargo bench -p wasi-runtime --bench runtime_hot_paths -- --noplot
```

Run [`32143931729`](https://github.com/Ferrousagent/ferrous/actions/runs/32143931729)
measured the following Criterion medians on `ubuntu-latest`:

| Hot path | Median |
| --- | ---: |
| Command request validation | 124.10 ns |
| Allowlisted environment selection | 76.778 ns |
| Loopback network policy check | 2.5681 ns |
| Bounded pipe write and drain | 245.47 ns |

These are policy and streaming primitives, not end-to-end execution latency.
Process creation, PTY setup, Wasmtime compilation, guest startup, and UI
transport are intentionally separate measurements and are not claimed to be
sub-millisecond.

## Remaining platform scope

- Windows ConPTY and any future non-Unix native policy adapter remain
  fail-closed until they have equivalent process-tree and policy tests.
- The human-facing terminal renderer (wterm or another renderer) remains a
  later UI-phase adapter. The backend contract is renderer-independent.
- Passing CI and microbenchmarks are evidence of tested behavior, not proof of
  zero bugs, formal verification, or bank/healthcare certification. Production
  deployment still requires independent security review, threat modeling,
  OS-level sandboxing, secrets governance, and operational controls.
