# Archive I/O failure policy

Approved 2026-09-23: stop archive admission on uncertain I/O, alert, preserve
evidence, and remain blocked until recovery succeeds. Do not silently continue
appending or sealing after a failed frame write.

## Behavior

The writer serializes writes, flushes and sealing. An error latches failure;
subsequent operations return errors, including automatic sealing on drop.
An unwind is also latched before it is rethrown. Already durable sealed segments
remain valid; this does not stop their downstream processing.

Before detaching a segment for seal, the writer fsyncs a
`<prefix>.recovery-required` checkpoint and its parent directory. It removes
that checkpoint only after the synchronous seal succeeds. A write failure also
attempts to persist the marker. If a full disk prevents marker creation, the
retained `.notepack.open` file still prevents startup. Startup refuses either
condition. The live daemon checks this before opening RocksDB and remains
resident with admission disabled and monitoring available until an operator
stops it. It does not poll for marker removal or automatically resume. This is
intentionally conservative after a process/machine crash.

An in-process writer fault stops live intake and signals shutdown of the
in-process reconciliation task. Later events are not counted as novel relay
traffic. A dedupe lookup error rejects that event without terminating healthy
live subscriptions; this is not a promise that the event will be redelivered.

Buffered bytes may flush when a failed writer is dropped, but the file is not
promoted to a sealed archive segment. Preserve it unchanged for inspection.
Automatic compression runs only after the authoritative uncompressed segment
has been sealed; compression fallback is distinct from uncertain frame I/O.

## Operator recovery gate

This is a recovery **acceptance checklist**, not an executable repair procedure.
The current tools do not implement recovery for every complete `.open` orphan or
post-rename failure. Do not deploy this change until those procedures are tested
and their reconciliation has been verified. The live service now remains
observable instead of exiting repeatedly on a known recovery obligation. Preserve the
blocked state; do not remove markers just to restore availability.

1. Inspect logs, disk/inode space, filesystem health and failed paths. Do not
   delete evidence, clear the dedupe database, or repeatedly restart ingestion.
2. Stop the ingester before inspecting or recovering mutable archive files.
   Record the marker, `.open` and any corresponding sealed file identities.
3. Resolve the underlying storage fault. Preserve an evidence copy before any
   repair. Enumerate complete length-prefixed frames; validate event IDs and
   signatures and identify any incomplete trailing frame. Never rename a raw
   `.open` file to a sealed name merely to make startup pass.
4. Using an incident-specific, verified recovery procedure, recover valid events and reconcile
   durable dedupe state, and account explicitly for incomplete/invalid data.
   For post-rename failure, verify the sealed file and repair missing downstream
   indexing/inventory notifications without deleting the authoritative file.
5. Only after recovery and reconciliation succeed, move the recovered `.open`
   evidence out of the active segment namespace, preserve the marker as incident
   evidence, and remove its active-path entry. This is an explicit operator gate;
   no automatic recovery/marker-clearing tool is provided by this change.
6. Restart once; verify fresh sealing, matching ClickHouse/Parquet counts,
   archive sync health and stable disk reserve. Check alert resolution.

Deploying this policy requires a read-only preflight for existing `.open` files
and recovery markers. Existing orphan files must be accounted for first. Do not
deploy simply because unit tests pass.

## Alerting

`archive_recovery_required = 1` and `archive_admission_failures_total` expose the
latched fault. Rules are in `ops/production/prometheus/archive-alerts.yml`, with
a separate ingester-unavailable rule for process/startup failure. These changes
are not deployed automatically. Verify rule loading and the site's notification
receiver routing before claiming operator notifications are delivered.
The selected notification system is Uptime Kuma. Configure an HTTP monitor for
`http://<private-ingester-address>:9091/health/archive-admission`, accepting only
HTTP 200. It returns 503 during startup, recovery-required state, and shutdown.
This endpoint is served on the existing metrics port; do not expose that port
publicly just to monitor it. A disabled metrics port also disables this endpoint.

This is an **archive-admission** check, not a guarantee that relays are delivering
events or that ClickHouse, Parquet, and remote storage are current. Keep separate
freshness/storage checks. The metrics listener must bind successfully before
ingestion starts; bind errors are returned rather than hidden in a detached task.

During deployment, configure Kuma's notification destination and prove both
firing and resolution against a non-production fault fixture. A failed process
or unreachable host must also make the monitor fail. Do not claim notifications
are delivered until that test passes. The Prometheus rules remain useful for
diagnostics, but have no configured Alertmanager receiver.
