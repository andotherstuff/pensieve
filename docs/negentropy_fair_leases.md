# Fair lease selector

`JobLedger::lease_next_fair(now, ttl)` remains opt-in. The existing `lease_next`
retains its original behavior. The default-off isolated runtime now uses this
selector through the parent owner after preflight; see
[the runtime contract](negentropy_runtime.md). No production activation is added.

The selector considers only enabled planner relay identities, at most 32. Fresh
work is a queued root with attempt zero. Retained gaps are retry-wait jobs or
queued children/requeued attempts. Blocked, split parents, complete and awaiting
jobs cannot be leased. Disabled relay jobs and receipts remain untouched; enabling
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
