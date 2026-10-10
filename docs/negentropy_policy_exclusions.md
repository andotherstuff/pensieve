# Durable recipient-policy exclusions

This slice remains default-off and does not authorize production deployment.

The isolated parent uses the same kind-1059/lowercase-p-tag recipient predicate
as live ingestion. The worker cannot choose an exclusion or bypass validation:
the parent verifies the event signature, ID, window, lease and frame identity
before applying its configured policy. It persists the exclusion before ACK,
without touching the archive writer or global dedupe index for that event.

## Accounting and recovery

Ledger schema 8 adds canonical policy keys plus a fingerprint, an event-ID/timestamp exclusion index,
and separate per-attempt `excluded` counts. Exclusion rows retain the first
job/attempt and the pinned policy identifies the reason (`giftwrap_recipient_v1`).
No blocked payload is retained. Archived counts never include exclusions.
Completion requires a valid protocol summary plus every receipt resolved by
either an archive marker or a recorded exclusion. A summary alone is insufficient.

The policy transaction inserts the exclusion, removes the current unresolved
receipt and increments the exclusion count atomically. A failed transaction
returns no ACK and retains the receipt. A crash after commit but before ACK
retains the exclusion; prior-attempt receipts for that ID are resolved during
bounded maintenance, but no unfinished attempt becomes protocol-complete.
Exclusions are recorded even if archive recovery is latched: they require no
archive I/O. Recovery still blocks job completion and normal event admission.

Retries and later sweeps combine archive-only inventory with the exclusion index
for reconciliation only. The combined inventory is sorted, deduplicated, and
capped at 250,000 IDs. A larger union requests normal window splitting, never a
truncated successful inventory. At most two capped input vectors are combined;
the exclusion query examines at most 250,001 rows through a timestamp index.
This avoids re-fetching previously excluded IDs, not transmission of previously
unseen spam. The `WITHOUT ROWID` cache holds at most 100,000 IDs, reduced for
smaller ledger budgets using a conservative 1 KiB/ID allowance within one eighth
of the configured budget. The existing physical SQLite/WAL budget still applies.
At capacity, admission atomically evicts one oldest ID that backs no unresolved
receipt. The bounded cache/time index and receipt-ID index constrain that search;
IDs backing unfinished receipts are never evicted. If all entries are pinned,
the transaction fails without ACK or loss of proof until maintenance releases
those receipts. Attempt summaries and excluded counts are never pruned. Eviction
can cause later refetch/re-exclusion, not false archival or completion claims.

`negentropy_policy_exclusions_retained` and `negentropy_policy_exclusions_capacity`
report cache occupancy and its cap; `negentropy_policy_exclusions_evicted_total`
counts committed evictions. This cache cannot grow with the full spam history.

## Compatibility

Wire protocol 2 explicitly distinguishes `admitted` from `policy_excluded` ACKs.
Neither says an event was archived. Mixed worker/parent versions fail closed.
The schema-7 upgrade is transactional and preserves jobs, attempts and receipts;
older binaries reject schema 8. Keep a ledger backup before a future rollout.

Policy keys are normalized, sorted and deduplicated before fingerprinting, with
a versioned semantics prefix. Every open verifies the stored keys against their
fingerprint. Adding recipients is accepted transactionally because all prior
claims remain valid, including upgrading an initially empty policy. Removing or
replacing a recipient refuses isolated startup. Such removal needs an explicit
recovery plan for previously excluded events; there is no automatic removal
migration in this slice. Live ingestion retains its independently configured
policy. Do not delete the ledger to bypass this check.

## Verification

Regression coverage includes mixed admitted/excluded traffic, no archive/dedupe
mutation for exclusions, transaction failure, lost ACK, reopen/retry accounting,
older-attempt obligations, schema-7 migration, safe policy expansion, removal and
corruption rejection, bounded cache churn, pinned-proof retention and exclusions
during latched archive recovery.
Linux process isolation and end-to-end rehearsal are separate from production
activation; no deployment occurs as part of this PR.
