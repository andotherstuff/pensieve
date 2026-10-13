# Fair lease selector

`JobLedger::lease_next_fair(now, ttl)` remains opt-in. The existing `lease_next`
retains its original behavior. The default-off isolated runtime now uses this
selector through the parent owner after preflight; see
[the runtime contract](negentropy_runtime.md). No production activation is added.

The selector considers only enabled planner relay identities, at most 32. Fresh
work is a queued root with attempt zero. Retained gaps are retry-wait jobs or
queued children/requeued attempts. Blocked, split parents, complete and awaiting
jobs cannot be leased. The owner's lifecycle turn runs before each lease, so a
retry-wait gap whose relay persistently withholds the same advertised IDs is
completed by a [relay-unavailable exclusion](negentropy_continuous_readiness.md#relay-unavailable-exclusions)
(job 768, 2026-10-13) instead of being leased again; it then stops occupying a
gap slot in the rotation. Disabled relay jobs and receipts remain untouched; enabling
the identity makes due jobs eligible again.

A singleton persists the preferred next class and independent relay cursors for
fresh and gap work. The preferred class is tried first, in round-robin relay order;
the other class is a fallback when none are due. A committed lease switches preference
to the opposite class and advances only the selected class's relay cursor. Empty,
blocked or failed selections do not advance anything. Separate cursors avoid a
disjoint fresh/gap relay set resetting the other class's fairness.

Within one relay/class the earliest eligibility deadline wins, then job ID. This
is **not** strict ordering by event timestamp, nor a throughput/freshness guarantee.
At most 64 indexed due-head queries run (two classes times 32 relays), each with
an exact partial-index predicate and no temporary sorting. The existing one-active
lease, two-awaiting limit and ledger admission budgets still apply. Expired active
leases must be recovered explicitly; the selector never steals them.

Candidate selection, capability creation, attempt increment and rotation update
commit in one transaction. Any error leaves both obligation and rotation intact.
The caller must supply execution-time Unix seconds; backward time cannot lease a
future-due gap. Clock jumps and storage latency are not monotonic-clock guarantees.

Fresh prototype schema 7 is required; older versions fail closed without migration
or deletion. Preserve real older obligations for an explicit recovery decision.

## Leasing the preflighted candidate

The runtime does not call `lease_next_fair` directly. The parent owner first takes
`fair_candidate(now)`, checks that candidate's archived inventory (which can take
minutes under live load), and then leases **that same job** with
`lease_fair_candidate`. Fair selection is decided when the candidate is chosen; the
lease commits the same attempt increment, capability and class-cursor rotation as
`lease_next_fair` would have for it. If the durable row no longer matches the
candidate snapshot (state, attempt, eligibility or relay), its relay was disabled,
it is not due at the execution-time clock, or the one-lease/two-awaiting gates now
block leasing, nothing is consumed and selection runs again on a later tick.

Before this, the parent re-ran fair selection after the scan and treated a
different result as `InvalidRequest`, after the different job's lease had already
committed. A retry-wait gap that became due during the scan therefore stopped the
whole runtime (production, 2026-10-13; see
[continuous readiness](negentropy_continuous_readiness.md#incident-2026-10-13-runtime-stopped-on-a-lease-race)).
