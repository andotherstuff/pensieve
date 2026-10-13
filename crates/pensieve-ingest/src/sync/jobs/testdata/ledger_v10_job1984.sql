-- Schema-10 negentropy job ledger as written by ingest-85a2402 (master at
-- 85a2402, before carried exclusions), shaped like production on 2026-10-13:
-- ledger_v9_job768.sql opened by JobLedger::open (schema 9 -> 10), job 768
-- excluded at 12:48 UTC (1791895680) with the single withheld ID
-- 089d1fdc...6b65, then the same wss://premium.primal.net window
-- 1791222300..1791223199 enqueued again as job 1984 (as the rolling planner's
-- 15:15 UTC pass did) and failed once: 17 events received and archived, report
-- `unavailable` naming only that ID. Dumped with sqlite3 .dump, which omits
-- the identity pragmas appended at the end.
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE jobs (
                            id INTEGER PRIMARY KEY, sweep TEXT NOT NULL, relay TEXT NOT NULL,
                            since INTEGER NOT NULL CHECK(since>=0), until INTEGER NOT NULL CHECK(until>=since),
                            parent INTEGER REFERENCES jobs(id), state TEXT NOT NULL CHECK(state IN ('queued','leased','awaiting_durability','complete','retry_wait','blocked','split')),
                            attempt INTEGER NOT NULL DEFAULT 0 CHECK(attempt>=0), token BLOB, expires_at INTEGER,
                            next_eligible INTEGER NOT NULL DEFAULT 0, reason TEXT,
                            scan_attempt INTEGER NOT NULL DEFAULT 0, scan_sequence INTEGER NOT NULL DEFAULT 0, completed_at INTEGER, recorded_at INTEGER NOT NULL DEFAULT 0, policy_identity BLOB,
                            UNIQUE(sweep,relay,since,until),
                            CHECK((state='leased' AND token IS NOT NULL AND typeof(token)='blob' AND length(token)=32 AND expires_at IS NOT NULL) OR (state!='leased' AND token IS NULL AND expires_at IS NULL))
                        );
INSERT INTO jobs VALUES(1,'manual-fixture','wss://premium.primal.net',1791221400,1791222299,NULL,'complete',1,NULL,NULL,0,NULL,0,0,1791882106,1791882106,x'9dba8632ad4e7cdb649308f163a1d78c400d768012b13d08534eabbad1b0520f');
INSERT INTO jobs VALUES(768,'manual-fixture','wss://premium.primal.net',1791222300,1791223199,NULL,'complete',15,NULL,NULL,1791260671,'relay_unavailable',0,0,1791906226,1791882106,x'9dba8632ad4e7cdb649308f163a1d78c400d768012b13d08534eabbad1b0520f');
INSERT INTO jobs VALUES(1984,'manual-fixture-pass2','wss://premium.primal.net',1791222300,1791223199,NULL,'retry_wait',1,NULL,NULL,1791904725,'relay_failure',0,0,NULL,1791906226,x'9dba8632ad4e7cdb649308f163a1d78c400d768012b13d08534eabbad1b0520f');
CREATE TABLE attempts (
                        job INTEGER NOT NULL REFERENCES jobs(id), attempt INTEGER NOT NULL,
                        received INTEGER NOT NULL, bytes INTEGER NOT NULL, digest BLOB NOT NULL,
                        protocol_done INTEGER NOT NULL DEFAULT 0,
                        archived INTEGER NOT NULL DEFAULT 0 CHECK(archived>=0 AND archived<=received), excluded INTEGER NOT NULL DEFAULT 0
    CHECK(excluded>=0 AND excluded+archived<=received),
                        PRIMARY KEY(job,attempt)
                    );
INSERT INTO attempts VALUES(1,1,0,0,x'a721ae21523764c0a9f2c93a3c7e277aed5f435a18c0bf6d0d0dc009ac443bfe',1,0,0);
INSERT INTO attempts VALUES(768,1,20,8210,x'8725e719dcd842da3d0b9a79c2feeaaf40125ae9022f2fdeadcc9ede40df5342',0,20,0);
INSERT INTO attempts VALUES(768,2,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,3,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,4,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,5,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,6,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,7,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,8,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,9,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,10,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,11,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,12,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,13,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,14,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(768,15,17,6953,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
INSERT INTO attempts VALUES(1984,1,17,6970,x'8a25e2d134a2fbd8d399186393338f27fb3f90ca56679a5b79d2abc962a877b9',0,17,0);
CREATE TABLE receipts (
                        job INTEGER NOT NULL, attempt INTEGER NOT NULL, sequence INTEGER NOT NULL,
                        event_id BLOB NOT NULL, created_at INTEGER NOT NULL,
                        frame_bytes INTEGER NOT NULL, frame_digest BLOB NOT NULL,
                        PRIMARY KEY(job,attempt,sequence),
                        FOREIGN KEY(job,attempt) REFERENCES attempts(job,attempt)
                    );
CREATE TABLE receipt_totals (
                        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                        retained INTEGER NOT NULL CHECK(retained>=0),
                        scan_job INTEGER NOT NULL DEFAULT 0 CHECK(scan_job>=0)
                    );
INSERT INTO receipt_totals VALUES(1,0,0);
CREATE TABLE failure_reports (
    job INTEGER NOT NULL REFERENCES jobs(id), attempt INTEGER NOT NULL,
    report TEXT NOT NULL CHECK(length(report)<=32768), reported_at INTEGER, PRIMARY KEY(job,attempt)
);
INSERT INTO failure_reports VALUES(768,1,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,2,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,3,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,4,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,5,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,6,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,7,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,8,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,9,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,10,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,11,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,12,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,13,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,14,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(768,15,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',NULL);
INSERT INTO failure_reports VALUES(1984,1,'{"kind":"unavailable","missing_count":1,"sample":[[8,157,31,220,45,28,240,240,59,229,132,64,20,55,138,63,43,204,160,209,9,38,25,167,14,214,248,107,124,40,107,101]]}',1791904651);
CREATE TABLE planner_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 sequence INTEGER NOT NULL CHECK(sequence>=0),
 after_relay INTEGER NOT NULL CHECK(after_relay>=0)
);
INSERT INTO planner_state VALUES(1,0,0);
CREATE TABLE planner_relays (
 id INTEGER PRIMARY KEY, relay TEXT NOT NULL UNIQUE,
 enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
 sequence INTEGER NOT NULL DEFAULT 0 CHECK(sequence>=0),
 upper INTEGER NOT NULL DEFAULT 0 CHECK(upper>=0),
 next INTEGER NOT NULL DEFAULT 0 CHECK(next>=0 AND next<=upper)
);
INSERT INTO planner_relays VALUES(1,'wss://premium.primal.net',1,0,0,0);
CREATE TABLE lease_rotation (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 next_class INTEGER NOT NULL CHECK(next_class IN (0,1)),
 after_fresh_relay INTEGER NOT NULL CHECK(after_fresh_relay>=0),
 after_gap_relay INTEGER NOT NULL CHECK(after_gap_relay>=0)
);
INSERT INTO lease_rotation VALUES(1,0,0,0);
CREATE TABLE admission_policy (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    fingerprint BLOB NOT NULL CHECK(length(fingerprint)=32),
    recipients TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(reason='giftwrap_recipient_v1')
);
INSERT INTO admission_policy VALUES(1,x'9dba8632ad4e7cdb649308f163a1d78c400d768012b13d08534eabbad1b0520f','[]','giftwrap_recipient_v1');
CREATE TABLE policy_exclusions (
    event_id BLOB PRIMARY KEY CHECK(length(event_id)=32),
    created_at INTEGER NOT NULL CHECK(created_at>=0),
    first_job INTEGER NOT NULL, first_attempt INTEGER NOT NULL,
    FOREIGN KEY(first_job,first_attempt) REFERENCES attempts(job,attempt)
) WITHOUT ROWID;
CREATE TABLE exclusion_totals (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    retained INTEGER NOT NULL CHECK(retained>=0)
);
INSERT INTO exclusion_totals VALUES(1,0);
CREATE TABLE lifecycle (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 high_job INTEGER NOT NULL, scan_root INTEGER NOT NULL DEFAULT 0,
 last_success INTEGER, activated_at INTEGER NOT NULL
);
INSERT INTO lifecycle VALUES(1,1984,0,1791906226,1791882106);
CREATE TABLE completed_coverage (
 relay TEXT NOT NULL, day INTEGER NOT NULL, policy_identity BLOB NOT NULL,
 windows BLOB NOT NULL CHECK(length(windows)=12),
 received INTEGER NOT NULL, archived INTEGER NOT NULL, excluded INTEGER NOT NULL,
 lease_grants INTEGER NOT NULL, failures INTEGER NOT NULL,
 compacted_at INTEGER NOT NULL, relay_unavailable INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(relay,day,policy_identity)
) WITHOUT ROWID;
CREATE TABLE relay_unavailable_exclusions (
 id INTEGER PRIMARY KEY,
 event_id BLOB NOT NULL CHECK(length(event_id)=32),
 job INTEGER NOT NULL, relay TEXT NOT NULL,
 since INTEGER NOT NULL CHECK(since>=0), until INTEGER NOT NULL CHECK(until>=since),
 first_attempt INTEGER NOT NULL CHECK(first_attempt>=1),
 last_attempt INTEGER NOT NULL CHECK(last_attempt>=first_attempt),
 attempts INTEGER NOT NULL CHECK(attempts>=1 AND attempts<=last_attempt-first_attempt+1),
 first_reported_at INTEGER, last_reported_at INTEGER,
 span_secs INTEGER NOT NULL CHECK(span_secs>=0),
 excluded_at INTEGER NOT NULL, resolved_at INTEGER,
 reason TEXT NOT NULL CHECK(reason='relay_unavailable'),
 UNIQUE(job,event_id)
);
INSERT INTO relay_unavailable_exclusions VALUES(1,x'089d1fdc2d1cf0f03be5844014378a3f2bcca0d1092619a70ed6f86b7c286b65',768,'wss://premium.primal.net',1791222300,1791223199,1,15,15,NULL,NULL,32580,1791895680,NULL,'relay_unavailable');
CREATE TRIGGER exclusion_added AFTER INSERT ON policy_exclusions BEGIN
    UPDATE exclusion_totals SET retained=retained+1 WHERE singleton=1;
END;
CREATE TRIGGER exclusion_removed AFTER DELETE ON policy_exclusions BEGIN
    UPDATE exclusion_totals SET retained=retained-1 WHERE singleton=1;
END;
CREATE TRIGGER job_recorded AFTER INSERT ON jobs BEGIN
 UPDATE jobs SET recorded_at=unixepoch(),
 policy_identity=(SELECT fingerprint FROM admission_policy WHERE singleton=1)
 WHERE id=NEW.id;
END;
CREATE TRIGGER job_completed AFTER UPDATE OF state ON jobs
WHEN NEW.state='complete' AND OLD.state!='complete' BEGIN
 UPDATE jobs SET completed_at=unixepoch() WHERE id=NEW.id;
 UPDATE lifecycle SET last_success=unixepoch() WHERE singleton=1;
END;
CREATE UNIQUE INDEX one_active_lease ON jobs(state) WHERE state='leased';
CREATE INDEX due_jobs ON jobs(state,next_eligible,id);
CREATE INDEX child_jobs ON jobs(parent);
CREATE INDEX unresolved_jobs ON jobs(id)
                            WHERE state IN ('awaiting_durability','retry_wait','blocked','split');
CREATE INDEX planner_unresolved_roots ON jobs(relay,id) WHERE state!='complete' AND parent IS NULL;
CREATE INDEX fresh_due ON jobs(relay,next_eligible,id)
 WHERE state='queued' AND parent IS NULL AND attempt=0;
CREATE INDEX gap_due ON jobs(relay,next_eligible,id)
 WHERE state='retry_wait' OR (state='queued' AND (parent IS NOT NULL OR attempt>0));
CREATE INDEX policy_exclusions_time ON policy_exclusions(created_at,event_id);
CREATE INDEX receipts_policy_id ON receipts(event_id);
CREATE INDEX retention_roots ON jobs(id) WHERE parent IS NULL AND state='complete';
CREATE INDEX pending_recorded ON jobs(recorded_at) WHERE state!='complete';
CREATE INDEX exclusion_owner ON policy_exclusions(first_job);
CREATE INDEX coverage_age ON completed_coverage(compacted_at);
CREATE INDEX relay_unavailable_unresolved ON relay_unavailable_exclusions(id)
 WHERE resolved_at IS NULL;
COMMIT;
PRAGMA application_id=1347308081;
PRAGMA user_version=10;
