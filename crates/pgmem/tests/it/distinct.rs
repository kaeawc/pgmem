use crate::support::setup;

fn distinct_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE events (id int PRIMARY KEY, region text NOT NULL, kind text NOT NULL);
         INSERT INTO events (id, region, kind) VALUES
             (1, 'east', 'view'),
             (2, 'east', 'view'),
             (3, 'east', 'click'),
             (4, 'west', 'view'),
             (5, 'west', 'view');",
    )
    .expect("distinct setup");
}

#[test]
fn distinct_single_column() {
    let (_db, mut c) = setup();
    distinct_setup(&mut c);
    let mut got: Vec<String> = c.query("SELECT DISTINCT region FROM events", &[]).expect("select").iter().map(|r| r.get(0)).collect();
    got.sort();
    assert_eq!(got, ["east", "west"]);
}

#[test]
fn distinct_multi_column() {
    let (_db, mut c) = setup();
    distinct_setup(&mut c);
    let rows = c.query("SELECT DISTINCT region, kind FROM events", &[]).expect("select");
    let mut got: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    got.sort();
    let want = [("east", "click"), ("east", "view"), ("west", "view")].map(|(a, b)| (a.to_owned(), b.to_owned()));
    assert_eq!(got, want);
}

#[test]
fn distinct_on_empty() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE empty (n int)").expect("create");
    let rows = c.query("SELECT DISTINCT n FROM empty", &[]).expect("select");
    assert_eq!(rows.len(), 0);
}

#[test]
fn distinct_composes_with_order() {
    let (_db, mut c) = setup();
    distinct_setup(&mut c);
    let rows = c.query("SELECT DISTINCT region FROM events ORDER BY region DESC", &[]).expect("select");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["west", "east"]);
}
