use crate::support::setup;

fn string_agg_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE tags (id int PRIMARY KEY, name text NOT NULL, kind text NOT NULL);
         INSERT INTO tags (id, name, kind) VALUES (1, 'red', 'color'), (2, 'blue', 'color'), (3, 'fast', 'speed');",
    )
    .expect("setup");
}

/// The parts of a joined string, sorted (aggregation order isn't guaranteed without ORDER BY).
fn sorted_parts(s: &str, sep: char) -> Vec<&str> {
    let mut parts: Vec<&str> = s.split(sep).collect();
    parts.sort();
    parts
}

#[test]
fn string_agg_scalar() {
    let (_db, mut c) = setup();
    string_agg_setup(&mut c);
    let got: String = c.query_one("SELECT string_agg(name, ',') FROM tags", &[]).expect("query").get(0);
    assert_eq!(sorted_parts(&got, ','), ["blue", "fast", "red"], "got {got:?}, want a permutation of red,blue,fast");
}

#[test]
fn string_agg_grouped() {
    let (_db, mut c) = setup();
    string_agg_setup(&mut c);
    let rows = c.query("SELECT kind, string_agg(name, '|') FROM tags GROUP BY kind ORDER BY kind", &[]).expect("query");
    let got: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].0, "color");
    assert_eq!(sorted_parts(&got[0].1, '|'), ["blue", "red"], "color names = {:?}", got[0].1);
    assert_eq!(got[1], ("speed".to_owned(), "fast".to_owned()));
}

/// All-NULL (here: empty) input gives NULL.
#[test]
fn string_agg_all_nulls_is_null() {
    let (_db, mut c) = setup();
    string_agg_setup(&mut c);
    let got: Option<String> = c.query_one("SELECT string_agg(name, ',') FROM tags WHERE id < 0", &[]).expect("query").get(0);
    assert_eq!(got, None);
}
