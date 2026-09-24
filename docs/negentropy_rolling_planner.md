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
identity only when the upper boundary advances. The namespace is reserved from
manual enqueue. A backward clock cannot start an older sweep; it does not prevent
resuming already-frozen work. Sequence exhaustion is an error, never reuse.

One transaction enqueues at most 32 roots and persists each relay cursor plus the
global sequence/round-robin position. Planning pauses at 32 queued root jobs for
currently enabled relay identities. Existing manually queued roots for enabled
relays count too; split children do not. Disabled relay backlog is preserved but
does not prevent planning for replacement relays. This cap bounds active planning,
not outstanding obligations: disabled, failed
jobs and split lineage remain durable. Errors, including a failure after job
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
