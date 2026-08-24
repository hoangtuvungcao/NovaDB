use novadb_core::NovaDb;
use novadb_server::RelayStore;

fn notes_database() -> NovaDb {
    let database = NovaDb::open_in_memory().expect("open database");
    database
        .execute_batch(
            "CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT NOT NULL
            );",
        )
        .expect("create schema");
    database.enable_sync("notes", "id").expect("enable sync");
    database
}

#[test]
fn real_core_changes_round_trip_through_relay() {
    let source = notes_database();
    let target = notes_database();
    let relay = RelayStore::open_in_memory().expect("open relay");

    source
        .execute_batch(
            "INSERT INTO notes(id, title, body)
             VALUES ('n1', 'Xin chao', 'from source');",
        )
        .expect("insert source row");
    let outgoing = source.changes_after(0, 100).expect("source changes");
    assert_eq!(outgoing.len(), 1);

    let pushed = relay.push("team_notes", &outgoing).expect("push relay");
    assert_eq!(pushed.accepted, 1);
    let pulled = relay.pull("team_notes", 0, 100).expect("pull relay");
    assert_eq!(pulled.changes.len(), 1);

    let incoming = pulled
        .changes
        .iter()
        .map(|entry| entry.change.clone())
        .collect::<Vec<_>>();
    let report = target.apply_changes(&incoming).expect("apply target");
    assert_eq!(report.applied, 1);
    assert!(target.changes_after(0, 100).expect("target log").is_empty());
    assert_eq!(
        target
            .query("SELECT title, body FROM notes WHERE id='n1'")
            .expect("query target")
            .rows[0],
        serde_json::json!({"title": "Xin chao", "body": "from source"})
    );

    // Pulling a replica's own already-materialized mutation is harmless.
    let own_report = source.apply_changes(&incoming).expect("apply own change");
    assert_eq!(own_report.duplicates, 1);
}

#[test]
fn replicas_converge_independent_of_delivery_order() {
    let peer_a = notes_database();
    let peer_b = notes_database();
    peer_a
        .execute_batch("INSERT INTO notes VALUES ('shared', 'from A', 'alpha');")
        .expect("write peer A");
    peer_b
        .execute_batch("INSERT INTO notes VALUES ('shared', 'from B', 'beta');")
        .expect("write peer B");

    let change_a = peer_a.changes_after(0, 10).expect("A change").remove(0);
    let change_b = peer_b.changes_after(0, 10).expect("B change").remove(0);
    let first_order = notes_database();
    let second_order = notes_database();

    first_order
        .apply_changes(&[change_a.clone(), change_b.clone()])
        .expect("apply A then B");
    second_order
        .apply_changes(&[change_b, change_a])
        .expect("apply B then A");

    let first = first_order
        .query("SELECT id, title, body FROM notes")
        .expect("query first");
    let second = second_order
        .query("SELECT id, title, body FROM notes")
        .expect("query second");
    assert_eq!(first, second);
}

#[test]
fn delete_tombstone_propagates_between_replicas() {
    let source = notes_database();
    let target = notes_database();
    let relay = RelayStore::open_in_memory().expect("open relay");

    source
        .execute_batch(
            "INSERT INTO notes(id, title, body) VALUES ('d1', 'delete me', 'soon');",
        )
        .expect("insert");
    source
        .execute_batch("DELETE FROM notes WHERE id = 'd1';")
        .expect("delete");

    let changes = source.changes_after(0, 100).expect("source changes");
    assert_eq!(changes.len(), 2); // insert + delete

    relay.push("tombstone_db", &changes).expect("push");
    let pulled = relay.pull("tombstone_db", 0, 100).expect("pull");

    let incoming: Vec<_> = pulled.changes.iter().map(|e| e.change.clone()).collect();
    target.apply_changes(&incoming).expect("apply");

    let result = target
        .query("SELECT * FROM notes WHERE id = 'd1'")
        .expect("query");
    assert!(result.is_empty(), "deleted row should not appear");
}

#[test]
fn integer_primary_key_sync_round_trips_through_relay() {
    let make_db = || {
        let db = NovaDb::open_in_memory().expect("open");
        db.execute_batch(
            "CREATE TABLE counters (
                id INTEGER PRIMARY KEY,
                value INTEGER NOT NULL DEFAULT 0
            );",
        )
        .expect("schema");
        db.enable_sync("counters", "id").expect("sync");
        db
    };

    let source = make_db();
    let target = make_db();
    let relay = RelayStore::open_in_memory().expect("relay");

    source
        .execute_batch("INSERT INTO counters(id, value) VALUES (1, 42), (2, 99);")
        .expect("insert");

    let changes = source.changes_after(0, 100).expect("changes");
    relay.push("int_pk_db", &changes).expect("push");
    let pulled = relay.pull("int_pk_db", 0, 100).expect("pull");

    let incoming: Vec<_> = pulled.changes.iter().map(|e| e.change.clone()).collect();
    target.apply_changes(&incoming).expect("apply");

    let result = target
        .query("SELECT id, value FROM counters ORDER BY id")
        .expect("query");
    assert_eq!(result.len(), 2);
    assert_eq!(result.rows[0]["id"], 1);
    assert_eq!(result.rows[0]["value"], 42);
    assert_eq!(result.rows[1]["id"], 2);
    assert_eq!(result.rows[1]["value"], 99);
}

#[test]
fn duplicate_push_is_idempotent() {
    let source = notes_database();
    let relay = RelayStore::open_in_memory().expect("relay");

    source
        .execute_batch("INSERT INTO notes(id, title, body) VALUES ('dup', 'same', 'data');")
        .expect("insert");

    let changes = source.changes_after(0, 100).expect("changes");
    let first = relay.push("dedup_db", &changes).expect("first push");
    assert_eq!(first.accepted, 1);
    assert_eq!(first.duplicates, 0);

    let second = relay.push("dedup_db", &changes).expect("second push");
    assert_eq!(second.accepted, 0);
    assert_eq!(second.duplicates, 1);

    let pulled = relay.pull("dedup_db", 0, 100).expect("pull");
    assert_eq!(pulled.changes.len(), 1, "only one copy in relay");
}

#[test]
fn multi_table_sync_convergence() {
    let make_db = || {
        let db = NovaDb::open_in_memory().expect("open");
        db.execute_batch(
            "CREATE TABLE notes (id TEXT PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL);
             CREATE TABLE tags  (id TEXT PRIMARY KEY, name TEXT NOT NULL);",
        )
        .expect("schema");
        db.enable_sync("notes", "id").expect("sync notes");
        db.enable_sync("tags", "id").expect("sync tags");
        db
    };

    let peer_a = make_db();
    let peer_b = make_db();
    let relay = RelayStore::open_in_memory().expect("relay");

    peer_a
        .execute_batch(
            "INSERT INTO notes VALUES ('n1', 'Note A', 'from A');
             INSERT INTO tags VALUES ('t1', 'rust');",
        )
        .expect("peer A writes");
    peer_b
        .execute_batch(
            "INSERT INTO notes VALUES ('n2', 'Note B', 'from B');
             INSERT INTO tags VALUES ('t2', 'database');",
        )
        .expect("peer B writes");

    // Push A, then B
    let changes_a = peer_a.changes_after(0, 100).expect("A changes");
    relay.push("multi_db", &changes_a).expect("push A");
    let changes_b = peer_b.changes_after(0, 100).expect("B changes");
    relay.push("multi_db", &changes_b).expect("push B");

    // Pull all to both
    let pulled = relay.pull("multi_db", 0, 100).expect("pull all");
    let all_changes: Vec<_> = pulled.changes.iter().map(|e| e.change.clone()).collect();

    peer_a.apply_changes(&all_changes).expect("A apply");
    peer_b.apply_changes(&all_changes).expect("B apply");

    let notes_a = peer_a
        .query("SELECT id, title FROM notes ORDER BY id")
        .expect("A notes");
    let notes_b = peer_b
        .query("SELECT id, title FROM notes ORDER BY id")
        .expect("B notes");
    assert_eq!(notes_a, notes_b);
    assert_eq!(notes_a.len(), 2);

    let tags_a = peer_a
        .query("SELECT id, name FROM tags ORDER BY id")
        .expect("A tags");
    let tags_b = peer_b
        .query("SELECT id, name FROM tags ORDER BY id")
        .expect("B tags");
    assert_eq!(tags_a, tags_b);
    assert_eq!(tags_a.len(), 2);
}

#[test]
fn update_convergence_last_writer_wins() {
    let peer_a = notes_database();
    let peer_b = notes_database();

    // Both peers create same row, then update it
    peer_a
        .execute_batch("INSERT INTO notes VALUES ('shared', 'v1', 'initial');")
        .expect("A insert");
    peer_b
        .execute_batch("INSERT INTO notes VALUES ('shared', 'v1', 'initial');")
        .expect("B insert");

    // A updates first, then B (B has later HLC)
    peer_a
        .execute_batch("UPDATE notes SET title = 'from-A' WHERE id = 'shared';")
        .expect("A update");
    peer_b
        .execute_batch("UPDATE notes SET title = 'from-B' WHERE id = 'shared';")
        .expect("B update");

    let changes_a = peer_a.changes_after(0, 100).expect("A changes");
    let changes_b = peer_b.changes_after(0, 100).expect("B changes");

    // Apply to a fresh peer in both orders — should converge
    let fresh1 = notes_database();
    let fresh2 = notes_database();

    fresh1
        .apply_changes(&changes_a)
        .expect("apply A to fresh1");
    fresh1
        .apply_changes(&changes_b)
        .expect("apply B to fresh1");

    fresh2
        .apply_changes(&changes_b)
        .expect("apply B to fresh2");
    fresh2
        .apply_changes(&changes_a)
        .expect("apply A to fresh2");

    let result1 = fresh1
        .query("SELECT title FROM notes WHERE id = 'shared'")
        .expect("fresh1 query");
    let result2 = fresh2
        .query("SELECT title FROM notes WHERE id = 'shared'")
        .expect("fresh2 query");
    assert_eq!(result1, result2, "LWW must converge regardless of apply order");
}

#[test]
fn pull_pagination_with_continuation_cursor() {
    let source = notes_database();
    let relay = RelayStore::open_in_memory().expect("relay");

    for i in 1..=5 {
        source
            .execute_batch(&format!(
                "INSERT INTO notes VALUES ('p{i}', 'Page {i}', 'body');"
            ))
            .expect("insert");
    }

    let changes = source.changes_after(0, 100).expect("changes");
    relay.push("page_db", &changes).expect("push");

    // Pull page by page with limit 2
    let page1 = relay.pull("page_db", 0, 2).expect("page 1");
    assert_eq!(page1.changes.len(), 2);
    assert!(page1.has_more);

    let page2 = relay.pull("page_db", page1.cursor, 2).expect("page 2");
    assert_eq!(page2.changes.len(), 2);
    assert!(page2.has_more);

    let page3 = relay.pull("page_db", page2.cursor, 2).expect("page 3");
    assert_eq!(page3.changes.len(), 1);
    assert!(!page3.has_more);
}
