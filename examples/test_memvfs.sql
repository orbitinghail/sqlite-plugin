-- Load the memvfs extension and open a new connection using it
-- Build the memvfs extension using the following command:
--   cargo build --example memvfs --features dynamic

-- uncomment to enable verbose logs
-- .log stderr

.load target/debug/examples/libmemvfs.so

-- ============================================================
-- Test 1: default journal mode (rollback)
-- ============================================================
.print === journal_mode=delete ===
.open main.db
.mode table
.log stdout

pragma journal_mode;

.databases
.vfsinfo

CREATE TABLE t1(a, b);
INSERT INTO t1 VALUES(1, 2);
INSERT INTO t1 VALUES(3, 4);
SELECT * FROM t1;
pragma hello_vfs=1234;

select * from dbstat;

vacuum;
drop table t1;
vacuum;

select * from dbstat;

-- ============================================================
-- Test 2: WAL journal mode
-- ============================================================
.print
.print === journal_mode=wal ===
.open wal.db
.mode table

pragma journal_mode=wal;
pragma journal_mode;

.databases
.vfsinfo

CREATE TABLE t1(a, b);
INSERT INTO t1 VALUES(1, 2);
INSERT INTO t1 VALUES(3, 4);
SELECT * FROM t1;
pragma hello_vfs=1234;

select * from dbstat;

vacuum;
drop table t1;
vacuum;

select * from dbstat;
