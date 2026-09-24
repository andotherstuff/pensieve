# Rolling planner (inactive library)

`JobLedger::configure_planner` and `plan_rolling` add persisted lazy planning, not
a scheduler, lease selection policy, parent API, listener or process launcher.
The approved static systemd worker/idle-wait design is unchanged. No production
configuration, inventory replay or historical exploration is activated.

An explicit allowlist contains at most 32 entries; normalized duplicates collapse.
Empty disables planning. Removal disables only future planning for that relay;
its jobs and frozen sweep cursor remain. Re-adding resumes that cursor. The ledger
retains at most 128 distinct relay identities. Further configuration churn fails
closed atomically; it never deletes old gaps or silently replaces identities.
Pure removals/disabling bypass admission ceilings so a full ledger cannot prevent
stopping planning. Actual SQLite/filesystem failures still propagate. Adding or
re-enabling identities retains the admission checks.

Each relay freezes a sweep at `floor(now / 900) * 900` (exclusive upper bound) and
starts at `max(0, upper - 14 days)`. Windows have inclusive endpoints, normally
`[since, since + 899]`. There are no boundary gaps/overlaps within a sweep. A
completed sweep can be revisited under a new globally monotonic `rolling-v1:N`
identity when the nonzero upper boundary changes, including after a backward
clock correction. The namespace is reserved from manual enqueue. Corrections
never abandon unfinished frozen planning or its jobs; after that cursor finishes,
a distinct sequence permits returning to corrected time without waiting for a
mistaken future timestamp. Sequence exhaustion is an error, never reuse.

One transaction enqueues at most 32 roots and persists each relay cursor plus the
global sequence/round-robin position. Each relay pauses at 32 unresolved root jobs
across queued, leased, awaiting-durability, retry, blocked and split states. A
forced partial index and at-most-32-row probe keep quota checks bounded. A full
relay is skipped so healthy relays continue; failures do not reopen quota. Existing
manual roots count too; split children do not. With 32 enabled relays the planner
can retain at most 1,024 unresolved roots across those identities (manual enqueue
is independently bounded by the ledger ceilings). Disabled backlog is preserved
but cannot block replacement relays. Split descendants and disabled obligations
remain under lifetime ledger budgets; this is not a total-obligation cap.
Errors, including a failure after job
insertion but before cursor update, roll back the entire turn. No repeated full
14-day materialization occurs in a single turn. Round-robin planning is persisted;
fair leasing between fresh jobs and old gaps remains a separate slice.

The supplied timestamp must eventually come from the owner at command execution,
not before an async queue wait. The 15-minute boundary is a minimum engineering
cadence, not a claim that sweeping every interval of every relay every 15 minutes
is sustainable. No runtime timer is implemented here. Canary throughput, freshness
and backlog acceptance remain rollout gates.

The default ledger has a **100,000 lifetime retained-job ceiling**, including
completed jobs and split parents, and a 1 GiB admission budget. Repeated sweeps
will eventually stop at those ceilings. This slice does not promise indefinite
operation or invent a retention/deletion policy. Preservation of unresolved gaps
takes precedence over throughput; a measured terminal-summary/retention design
must precede indefinite operation.

This is fresh unshipped schema 6, adding planner metadata to schema 5. Older ledger
versions are rejected without mutation, not migrated or discarded. Preserve any
older ledger containing real work and stop for an explicit recovery decision.

Regressions cover exact boundaries, repeated sweeps, reopen/clock rollback,
round-robin ordering, allowlist removal/re-add/churn, queue backpressure, retained
gaps, sequence overflow, old schema refusal and insertion/cursor/budget rollback.
