//! Port of examples/analytics/analytics_test.go: window functions, arrays + ANY, array_agg and unnest.

use crate::support::{TestDb, setup, utc_ts};
use jiff::SignedDuration;
use postgres::Client;

const ADD_PLAYER: &str = "-- name: AddPlayer :one

INSERT INTO players (name) VALUES ($1) RETURNING id, name
";
const RECORD_MATCH: &str = "-- name: RecordMatch :one
INSERT INTO matches (player_id, region, score, played_at)
VALUES ($1, $2, $3, $4) RETURNING id, player_id, region, score, played_at
";
const TOP_SCORES_BY_REGION: &str = "-- name: TopScoresByRegion :many
SELECT region, player_id, score,
       row_number() OVER (PARTITION BY region ORDER BY score DESC, id) AS rn
FROM matches
ORDER BY region, rn
";
const RANK_BY_SCORE: &str = "-- name: RankByScore :many
SELECT player_id, score,
       rank() OVER (ORDER BY score DESC) AS rk
FROM matches
ORDER BY rk, player_id
";
const SCORES_ARRAY_PER_PLAYER: &str = "-- name: ScoresArrayPerPlayer :many
SELECT player_id, array_agg(score)::bigint[] AS scores
FROM matches
GROUP BY player_id
ORDER BY player_id
";
const PLAYERS_BY_IDS: &str = "-- name: PlayersByIDs :many
SELECT id, name FROM players
WHERE id = ANY($1::bigint[])
ORDER BY id
";
const EXPAND_IDS: &str = "-- name: ExpandIDs :many
SELECT u::bigint AS id
FROM unnest($1::bigint[]) AS u
ORDER BY u
";

fn setup_schema() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(include_str!("../../../../examples/analytics/schema.sql")).expect("schema");
    (db, c)
}

fn add_player(c: &mut Client, name: &str) -> i64 {
    c.query_one(ADD_PLAYER, &[&name]).expect("AddPlayer").get(0)
}

/// Three players with matches in two regions: east alice 100, bob 90, alice 70; west carol 200, alice 200, bob 100.
fn fixture(c: &mut Client) -> (i64, i64, i64) {
    let now = utc_ts(2024, 7, 15, 12, 0, 0);
    let (alice, bob, carol) = (add_player(c, "alice"), add_player(c, "bob"), add_player(c, "carol"));
    let matches =
        [(alice, "east", 100i32), (bob, "east", 90), (alice, "east", 70), (carol, "west", 200), (alice, "west", 200), (bob, "west", 100)];
    for (i, (player, region, score)) in matches.into_iter().enumerate() {
        let at = now + SignedDuration::from_mins(i64::try_from(i).expect("small"));
        c.query_one(RECORD_MATCH, &[&player, &region, &score, &at]).expect("RecordMatch");
    }
    (alice, bob, carol)
}

#[test]
fn top_scores_by_region() {
    let (_db, mut c) = setup_schema();
    fixture(&mut c);
    let rows: Vec<(String, i64, i32, i64)> =
        c.query(TOP_SCORES_BY_REGION, &[]).expect("TopScoresByRegion").iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3))).collect();
    for r in &rows {
        assert!((1..=3).contains(&r.3), "row {r:?}: rn out of range");
    }
    let count = |region: &str| rows.iter().filter(|r| r.0 == region).count();
    assert_eq!((count("east"), count("west")), (3, 3));
    let top = |region: &str| rows.iter().find(|r| r.0 == region && r.3 == 1).map(|r| r.2);
    assert_eq!((top("east"), top("west")), (Some(100), Some(200)));
}

#[test]
fn rank_by_score() {
    let (_db, mut c) = setup_schema();
    fixture(&mut c);
    let rows: Vec<(i64, i32, i64)> =
        c.query(RANK_BY_SCORE, &[]).expect("RankByScore").iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!((rows[0].2, rows[1].2), (1, 1), "first two ranks");
    assert_eq!((rows[0].1, rows[1].1), (200, 200), "first two scores");
    assert_eq!(rows[2].2, 3, "third rank (rank-with-gap)");
}

#[test]
fn scores_array_per_player() {
    let (_db, mut c) = setup_schema();
    let (alice, _, _) = fixture(&mut c);
    let rows = c.query(SCORES_ARRAY_PER_PLAYER, &[]).expect("ScoresArrayPerPlayer");
    let mut scores: Vec<i64> = rows.iter().find(|r| r.get::<_, i64>(0) == alice).expect("alice row").get(1);
    scores.sort_unstable();
    assert_eq!(scores, vec![70, 100, 200]);
}

#[test]
fn players_by_ids() {
    let (_db, mut c) = setup_schema();
    let (alice, _, carol) = fixture(&mut c);
    let names: Vec<String> = c.query(PLAYERS_BY_IDS, &[&vec![alice, carol]]).expect("PlayersByIDs").iter().map(|r| r.get(1)).collect();
    assert_eq!(names, vec!["alice", "carol"]);
}

#[test]
fn expand_ids() {
    let (_db, mut c) = setup_schema();
    let got: Vec<i64> = c.query(EXPAND_IDS, &[&vec![42i64, 7, 1024]]).expect("ExpandIDs").iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![7, 42, 1024]);
}
