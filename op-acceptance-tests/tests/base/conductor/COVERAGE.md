# Conductor acceptance coverage map

Tracks how the legacy `op-e2e/system/conductor` suite maps onto the acceptance
tests in this package (#21902). The legacy tests remain in place until the
acceptance tests have demonstrated CI stability; remove them only after parity
has held in CI.

All tests here run against `presets.NewMinimalWithConductors`: a three-node
op-node/EL cluster, one op-conductor per node, CL p2p mesh, Raft cluster
formed at startup with sequencing seeded on the bootstrap node. They are
skipped on kona-node until #21906 lands conductor support there.

## Ported behaviors

| Legacy test (op-e2e/system/conductor) | Behavior | Acceptance test |
| --- | --- | --- |
| `TestSequencerFailover_SetupCluster` | Cluster starts with exactly one active sequencer (the Raft leader's) | `TestConductorClusterStartsWithOneActiveSequencer` |
| `TestSequencerFailover_ConductorRPC` (TransferLeaderToServer) | Manual leadership transfer stops the old sequencer and starts the target | `TestLeadershipTransferMovesActiveSequencer` |
| implicit in `sequencer_failover_setup.go` (batcher + safe-head progression) | Unsafe block production continues after a transfer; nodes converge on the same hashes | `TestUnsafeChainAdvancesAfterLeadershipTransfer` |
| `TestSequencerFailover_ActiveSequencerDown` | Health-based failover to a healthy replacement when the active sequencer dies | `TestFailoverOnActiveSequencerFailure` |
| `TestSequencerFailover_DisasterRecovery_OverrideLeader` | Leader override forces a survivor to sequence after quorum loss; refusal without override; overridden conductor proxies traffic | `TestDisasterRecoveryLeaderOverride` |
| `TestSequencerFailover_DisasterRecovery_OverrideLeader` (proxy assertions) | Conductor RPC proxy serves only the active leader | `TestConductorProxyServesOnlyLeader` |
| `TestSequencerFailover_ConductorRPC` (AddServerAsVoter / RemoveServer) | Cluster membership add/remove round trip | `TestConductorClusterMembershipChanges` |
| `TestSequencerFailover_ConductorRPC` (version mismatch) | Stale configuration-version changes are refused | `TestConductorRejectsStaleMembershipVersion` |

## Intentionally left at a lower level

These parts of `TestSequencerFailover_ConductorRPC` exercise the Raft/RPC
surface exhaustively rather than a user-visible behavior, and are better
served by `op-conductor` unit/integration tests:

- `Pause` / `Resume` / `Active` state machine details.
- `AddServerAsNonvoter` and suffrage distinctions.
- Leader-only enforcement of every membership RPC (`node is not the leader`).
- `TransferLeader` (round-robin, no explicit target) — the targeted variant is
  covered by the leadership-transfer acceptance test.
- `Stop` RPC semantics (used, and thereby smoke-tested, by
  `TestDisasterRecoveryLeaderOverride` via `dsl.Conductor.Stop`).

## Evaluated separately (not conductor-specific)

From `op-e2e/system/conductor/system_adminrpc_test.go`:

- `TestStopStartSequencer` and `TestPostUnsafePayload` are CL conformance
  behaviors (admin API contract of a consensus node, HA or not). They are
  retained in op-e2e for now; they belong in shared CL-conformance acceptance
  coverage so they can run against both op-node and kona-node (#21906). The
  DSL already exposes the underlying actions (`StartSequencer`,
  `StopSequencer`, `PostUnsafePayload` on `dsl.L2CLNode`).
- `TestPersistSequencerStateWhenChanged`, `TestLoadSequencerStateOnStarted_*`
  cover sequencer-state persistence, which is unrelated to conductors and not
  wired in the devstack presets. Retained in op-e2e.
