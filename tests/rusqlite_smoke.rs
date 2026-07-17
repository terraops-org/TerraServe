#[test]
fn rusqlite_in_memory_roundtrip() {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE t(a INTEGER, b TEXT); INSERT INTO t VALUES (7,'x');")
        .unwrap();
    let (a, b): (i64, String) = c
        .query_row("SELECT a,b FROM t", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((a, b.as_str()), (7, "x"));
}
