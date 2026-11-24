use crate::support::setup;

type WinRow = (i32, String, i32, i64);

fn window_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE sales (id int PRIMARY KEY, region text NOT NULL, amount int NOT NULL);
         INSERT INTO sales (id, region, amount) VALUES
             (1, 'east', 100),
             (2, 'east', 90),
             (3, 'east', 100),
             (4, 'west', 200),
             (5, 'west', 50);",
    )
    .expect("setup");
}

fn collect_window(c: &mut postgres::Client, sql: &str) -> Vec<WinRow> {
    c.query(sql, &[]).expect(sql).iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3))).collect()
}

fn win(rows: &[(i32, &str, i32, i64)]) -> Vec<WinRow> {
    rows.iter().map(|&(id, region, amount, r)| (id, region.to_owned(), amount, r)).collect()
}

#[test]
fn window_row_number_partition_order() {
    let (_db, mut c) = setup();
    window_setup(&mut c);
    let mut got = collect_window(
        &mut c,
        "SELECT id, region, amount, row_number() OVER (PARTITION BY region ORDER BY amount DESC) AS rn FROM sales ORDER BY region, rn",
    );
    // ids 1 and 3 tie on amount 100: Postgres does not define which gets row_number 1, so check the
    // numbering sequence, then compare with the tied pair's ids put in id order.
    let numbering: Vec<(String, i64)> = got.iter().map(|r| (r.1.clone(), r.3)).collect();
    let want_numbering = [("east", 1), ("east", 2), ("east", 3), ("west", 1), ("west", 2)].map(|(r, n)| (r.to_owned(), n));
    assert_eq!(numbering, want_numbering);
    if got[0].0 > got[1].0 {
        let (a, b) = (got[0].0, got[1].0);
        got[0].0 = b;
        got[1].0 = a;
    }
    assert_eq!(got, win(&[(1, "east", 100, 1), (3, "east", 100, 2), (2, "east", 90, 3), (4, "west", 200, 1), (5, "west", 50, 2)]));
}

#[test]
fn window_rank_ties_share_rank() {
    let (_db, mut c) = setup();
    window_setup(&mut c);
    let got = collect_window(
        &mut c,
        "SELECT id, region, amount, rank() OVER (PARTITION BY region ORDER BY amount DESC) AS rk FROM sales ORDER BY region, id",
    );
    // east: ids 1/3 both 100 share rank 1, id 2 (90) gets rank 3 (gap).
    assert_eq!(got, win(&[(1, "east", 100, 1), (2, "east", 90, 3), (3, "east", 100, 1), (4, "west", 200, 1), (5, "west", 50, 2)]));
}

#[test]
fn window_dense_rank_no_gap() {
    let (_db, mut c) = setup();
    window_setup(&mut c);
    let got = collect_window(
        &mut c,
        "SELECT id, region, amount, dense_rank() OVER (PARTITION BY region ORDER BY amount DESC) AS dr FROM sales ORDER BY region, id",
    );
    // east: 100/100 share rank 1, 90 is rank 2 (no gap).
    assert_eq!(got, win(&[(1, "east", 100, 1), (2, "east", 90, 2), (3, "east", 100, 1), (4, "west", 200, 1), (5, "west", 50, 2)]));
}

#[test]
fn window_no_partition() {
    let (_db, mut c) = setup();
    window_setup(&mut c);
    let got = collect_window(&mut c, "SELECT id, region, amount, row_number() OVER (ORDER BY amount DESC) AS rn FROM sales ORDER BY rn");
    assert_eq!(got.len(), 5, "{got:?}");
    assert_eq!(got.iter().map(|r| r.3).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
    assert_eq!(got[0].2, 200);
    assert_eq!(got[4].2, 50);
}
