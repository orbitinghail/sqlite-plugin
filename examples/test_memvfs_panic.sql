-- Load the memvfs extension and open a new connection using it
-- Build the memvfs extension using the following command:
--   cargo build --example memvfs --features dynamic,std

-- uncomment to enable verbose logs
-- .log stderr

.load target/debug/examples/libmemvfs.so
.open main.db
.mode table
.log stdout

.databases
.vfsinfo

-- ensure that panics are handled
pragma memvfs_panic;

-- but they cause all future calls to also fail!
CREATE TABLE t1(a, b);
