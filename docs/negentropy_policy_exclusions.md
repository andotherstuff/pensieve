# Durable recipient-policy exclusions

This slice remains default-off and does not authorize production deployment.

The isolated parent uses the same kind-1059/lowercase-p-tag recipient predicate
as live ingestion. The worker cannot choose an exclusion or bypass validation:
the parent verifies the event signature, ID, window, lease and frame identity
before applying its configured policy. It persists the exclusion before ACK,
without touching the archive writer or global dedupe index for that event.

## Accounting and recovery

Ledger schema 8 adds a policy fingerprint, an event-ID/timestamp exclusion index,
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

Retries and later sweeps combine archive-only inventory with the exclusion index
for reconciliation only. The combined inventory is sorted, deduplicated, and
capped at 250,000 IDs. A larger union requests normal window splitting, never a
truncated successful inventory. At most two capped input vectors are combined;
the exclusion query examines at most 250,001 rows through a timestamp index.
This avoids re-fetching previously excluded IDs, not transmission of previously
unseen spam. Exclusion records share the existing 1-GiB ledger admission ceiling;
exhaustion pauses isolated work and preserves obligations, rather than deleting
history or growing an unbounded cache. No automatic exclusion cleanup is added.

## Compatibility

Wire protocol 2 explicitly distinguishes `admitted` from `policy_excluded` ACKs.
Neither says an event was archived. Mixed worker/parent versions fail closed.
The schema-7 upgrade is transactional and preserves jobs, attempts and receipts;
older binaries reject schema 8. Keep a ledger backup before a future rollout.

Policy keys are normalized, sorted and deduplicated before fingerprinting, with
a versioned semantics prefix. Every open checks the pinned fingerprint. Changing
or removing the block list on an existing ledger refuses isolated startup;
live ingestion retains its independently configured policy. Policy changes need
an explicit migration/recovery plan that preserves unfinished gaps. Do not delete
the ledger or remove the block merely to bypass this check.

## Verification

Regression coverage includes mixed admitted/excluded traffic, no archive/dedupe
mutation for exclusions, transaction failure, lost ACK, reopen/retry accounting,
older-attempt obligations, schema-7 migration and policy-change rejection.
Linux process isolation and end-to-end rehearsal are separate from production
activation; no deployment occurs as part of this PR.
