use crate::support::setup;

fn lateral_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE authors (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE posts (id int PRIMARY KEY, author_id int NOT NULL REFERENCES authors(id), title text NOT NULL, score int NOT NULL);
         INSERT INTO authors (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'carol');
         INSERT INTO posts (id, author_id, title, score) VALUES
             (10, 1, 'A1', 50),
             (11, 1, 'A2', 90),
             (12, 1, 'A3', 70),
             (20, 2, 'B1', 30),
             (21, 2, 'B2', 60);",
    )
    .expect("lateral setup");
    // carol has no posts.
}

#[test]
fn lateral_top_post_per_author() {
    let (_db, mut c) = setup();
    lateral_setup(&mut c);
    let rows = c
        .query(
            "SELECT a.id, a.name, top.title
             FROM authors a
             LEFT JOIN LATERAL (
                 SELECT title FROM posts p
                 WHERE p.author_id = a.id
                 ORDER BY p.score DESC LIMIT 1
             ) AS top ON true
             ORDER BY a.id",
            &[],
        )
        .expect("query");
    let got: Vec<(i32, String, Option<String>)> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!(
        got,
        vec![(1, "alice".to_owned(), Some("A2".to_owned())), (2, "bob".to_owned(), Some("B2".to_owned())), (3, "carol".to_owned(), None),]
    );
}

#[test]
fn lateral_cross_join_lateral_all_posts() {
    let (_db, mut c) = setup();
    lateral_setup(&mut c);
    let rows = c
        .query(
            "SELECT a.id, p.title
             FROM authors a
             CROSS JOIN LATERAL (
                 SELECT title FROM posts WHERE author_id = a.id ORDER BY id
             ) AS p
             ORDER BY a.id, p.title",
            &[],
        )
        .expect("query");
    let titles: Vec<String> = rows.iter().map(|r| r.get::<_, String>(1)).collect();
    let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(titles, ["A1", "A2", "A3", "B1", "B2"]);
    assert_eq!(ids, [1, 1, 1, 2, 2]);
}
