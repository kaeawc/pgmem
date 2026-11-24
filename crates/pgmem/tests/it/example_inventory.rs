//! Port of examples/inventory/inventory_test.go: sum() GROUP BY rollups, CTE + CASE, self-referential FK.

use crate::support::{TestDb, setup, utc_ts};
use jiff::Timestamp;
use postgres::Client;

const ADD_CATEGORY: &str = "-- name: AddCategory :one

INSERT INTO categories (parent_id, name) VALUES ($1, $2) RETURNING id, parent_id, name
";
const ADD_PRODUCT: &str = "-- name: AddProduct :one
INSERT INTO products (category_id, name, threshold)
VALUES ($1, $2, $3) RETURNING id, category_id, name, threshold
";
const ADD_WAREHOUSE: &str = "-- name: AddWarehouse :one
INSERT INTO warehouses (name) VALUES ($1) RETURNING id, name
";
const RECORD_TXN: &str = "-- name: RecordTxn :one
INSERT INTO stock_txns (product_id, warehouse_id, delta, occurred_at)
VALUES ($1, $2, $3, $4) RETURNING id, product_id, warehouse_id, delta, occurred_at
";
const CHILD_CATEGORIES: &str = "-- name: ChildCategories :many
SELECT id, parent_id, name FROM categories WHERE parent_id = $1 ORDER BY name
";
const LOW_STOCK_REPORT: &str = "-- name: LowStockReport :many
WITH on_hand AS (
    SELECT product_id, sum(delta)::int AS qty
    FROM stock_txns
    GROUP BY product_id
)
SELECT p.id, p.name,
       coalesce(o.qty, 0) AS qty,
       CASE WHEN coalesce(o.qty, 0) < p.threshold THEN 'low' ELSE 'ok' END AS status
FROM products p
LEFT JOIN on_hand o ON o.product_id = p.id
ORDER BY status, p.name
";
const ON_HAND_BY_PRODUCT: &str = "-- name: OnHandByProduct :many
SELECT product_id, sum(delta)::int AS qty
FROM stock_txns
GROUP BY product_id
ORDER BY product_id
";
const WAREHOUSE_ROLLUP: &str = "-- name: WarehouseRollup :many
SELECT warehouse_id, sum(delta)::int AS qty
FROM stock_txns
GROUP BY warehouse_id
ORDER BY warehouse_id
";

fn setup_schema() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(include_str!("../../../../examples/inventory/schema.sql")).expect("schema");
    (db, c)
}

struct Fixture {
    root: i64,
    leaf: i64,
    p1: i64,
    p2: i64,
    wh1: i64,
    wh2: i64,
}

fn now() -> Timestamp {
    utc_ts(2024, 7, 15, 12, 0, 0)
}

fn add_category(c: &mut Client, parent: Option<i64>, name: &str) -> i64 {
    c.query_one(ADD_CATEGORY, &[&parent, &name]).expect("AddCategory").get(0)
}

fn add_product(c: &mut Client, category: i64, name: &str, threshold: i32) -> i64 {
    c.query_one(ADD_PRODUCT, &[&category, &name, &threshold]).expect("AddProduct").get(0)
}

fn record_txn(c: &mut Client, product: i64, warehouse: i64, delta: i32) {
    c.query_one(RECORD_TXN, &[&product, &warehouse, &delta, &now()]).expect("RecordTxn");
}

fn fixture(c: &mut Client) -> Fixture {
    let root = add_category(c, None, "tools");
    let leaf = add_category(c, Some(root), "drills");
    let wh1: i64 = c.query_one(ADD_WAREHOUSE, &[&"north"]).expect("AddWarehouse north").get(0);
    let wh2: i64 = c.query_one(ADD_WAREHOUSE, &[&"south"]).expect("AddWarehouse south").get(0);
    let p1 = add_product(c, leaf, "hammer", 5);
    let p2 = add_product(c, leaf, "screwdriver", 10);
    record_txn(c, p1, wh1, 7);
    record_txn(c, p1, wh1, -3);
    record_txn(c, p2, wh2, 4);
    Fixture { root, leaf, p1, p2, wh1, wh2 }
}

/// LowStockReport as (name, qty, status).
fn query_low_stock(c: &mut Client) -> Vec<(String, i32, String)> {
    c.query(LOW_STOCK_REPORT, &[]).expect("LowStockReport").iter().map(|r| (r.get(1), r.get(2), r.get(3))).collect()
}

#[test]
fn on_hand_by_product() {
    let (_db, mut c) = setup_schema();
    let f = fixture(&mut c);
    let got: Vec<(i64, i32)> = c.query(ON_HAND_BY_PRODUCT, &[]).expect("OnHandByProduct").iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(f.p1, 4), (f.p2, 4)]);
}

#[test]
fn low_stock_report() {
    let (_db, mut c) = setup_schema();
    fixture(&mut c);
    let rows = query_low_stock(&mut c);
    assert_eq!(rows.len(), 2);
    for (name, qty, status) in &rows {
        assert_eq!(status, "low", "{name}: qty={qty}");
    }
}

#[test]
fn low_stock_report_ok_branch() {
    let (_db, mut c) = setup_schema();
    let f = fixture(&mut c);
    let wrench = add_product(&mut c, f.leaf, "wrench", 1);
    record_txn(&mut c, wrench, f.wh1, 50);
    let rows = query_low_stock(&mut c);
    let status = rows.iter().find(|r| r.0 == "wrench").map(|r| r.2.as_str());
    assert_eq!(status, Some("ok"));
}

#[test]
fn warehouse_rollup() {
    let (_db, mut c) = setup_schema();
    let f = fixture(&mut c);
    let got: Vec<(i64, i32)> = c.query(WAREHOUSE_ROLLUP, &[]).expect("WarehouseRollup").iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(f.wh1, 4), (f.wh2, 4)]);
}

#[test]
fn child_categories() {
    let (_db, mut c) = setup_schema();
    let f = fixture(&mut c);
    let got: Vec<(i64, String)> =
        c.query(CHILD_CATEGORIES, &[&Some(f.root)]).expect("ChildCategories").iter().map(|r| (r.get(0), r.get(2))).collect();
    assert_eq!(got, vec![(f.leaf, "drills".to_owned())]);
}
