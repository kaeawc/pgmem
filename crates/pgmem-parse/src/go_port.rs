//! Ports of the Go `postgres/parse` tests. pgmem-go's parser produced executable plans; this
//! parser produces a SQL-shaped tree and leaves desugaring (SERIAL, PRIMARY KEY implying
//! NOT NULL, RETURNING column names) to the binder, so these tests assert the same facts at
//! the level the facts now live, and the binder-level halves are covered by integration tests.

use crate::parse_one;
use pgmem_ir::{
    ColumnConstraintKind, Expr, InsertSource, JoinConstraint, JoinKind, ReferentialAction, SelectItem, SetExpr, Statement, TableRef,
};

fn stmt(sql: &str) -> Statement {
    parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn select(sql: &str) -> pgmem_ir::Select {
    match stmt(sql) {
        Statement::Query(q) => match q.body {
            SetExpr::Select(s) => *s,
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn first_expr(sql: &str) -> Expr {
    match select(sql).projection.into_iter().next() {
        Some(SelectItem::Expr { expr, .. }) => expr,
        other => panic!("{sql}: {other:?}"),
    }
}

fn binary(e: &Expr) -> (&str, &Expr, &Expr) {
    match e {
        Expr::Binary { op, left, right } => (op.as_str(), left.as_ref(), right.as_ref()),
        other => panic!("not a binary operator: {other:?}"),
    }
}

#[test]
fn parse_arithmetic_precedence() {
    let e = first_expr("SELECT a + b * c FROM t");
    let (op, _, right) = binary(&e);
    assert_eq!(op, "+");
    assert_eq!(binary(right).0, "*");
}

#[test]
fn parse_arithmetic_left_associativity() {
    let e = first_expr("SELECT a - b - c FROM t");
    let (op, left, _) = binary(&e);
    assert_eq!(op, "-");
    assert_eq!(binary(left).0, "-");
}

fn create_table(sql: &str) -> pgmem_ir::CreateTable {
    match stmt(sql) {
        Statement::CreateTable(ct) => *ct,
        other => panic!("{sql}: {other:?}"),
    }
}

fn has(col: &pgmem_ir::ColumnDef, f: impl Fn(&ColumnConstraintKind) -> bool) -> bool {
    col.constraints.iter().any(|c| f(&c.kind))
}

#[test]
fn parse_create_table_check_constraint() {
    for (sql, want_check) in [
        ("CREATE TABLE t (qty int CHECK (qty > 0))", true),
        ("CREATE TABLE t (qty int NOT NULL CHECK (qty > 0))", true),
        ("CREATE TABLE t (qty int CHECK (qty > 0) NOT NULL)", true),
        ("CREATE TABLE t (qty int NOT NULL)", false),
    ] {
        let ct = create_table(sql);
        let col = &ct.columns[0];
        assert_eq!(has(col, |k| matches!(k, ColumnConstraintKind::Check(_))), want_check, "{sql}");
        assert_eq!(col.ty.name, "int4", "{sql}");
    }
}

#[test]
fn parse_create_table_column_constraints() {
    // (sql, per column: (name, type, not null, unique-or-primary-key, primary key))
    type Want = (&'static str, &'static str, bool, bool, bool);
    let cases: [(&str, Vec<Want>); 4] = [
        ("CREATE TABLE t (id int PRIMARY KEY, name text)", vec![("id", "int4", false, true, true), ("name", "text", false, false, false)]),
        ("CREATE TABLE t (slug text UNIQUE, body text)", vec![("slug", "text", false, true, false), ("body", "text", false, false, false)]),
        (
            "CREATE TABLE t (a int NOT NULL UNIQUE, b int UNIQUE NOT NULL)",
            vec![("a", "int4", true, true, false), ("b", "int4", true, true, false)],
        ),
        ("CREATE TABLE t (n int)", vec![("n", "int4", false, false, false)]),
    ];
    for (sql, want) in cases {
        let ct = create_table(sql);
        assert_eq!(ct.columns.len(), want.len(), "{sql}");
        for (col, (name, ty, not_null, unique, pk)) in ct.columns.iter().zip(want) {
            assert_eq!(col.name, name, "{sql}");
            assert_eq!(col.ty.name, ty, "{sql}");
            assert_eq!(has(col, |k| matches!(k, ColumnConstraintKind::NotNull)), not_null, "{sql}: {name} NOT NULL");
            assert_eq!(
                has(col, |k| matches!(k, ColumnConstraintKind::Unique { .. } | ColumnConstraintKind::PrimaryKey)),
                unique,
                "{sql}: {name} unique"
            );
            assert_eq!(has(col, |k| matches!(k, ColumnConstraintKind::PrimaryKey)), pk, "{sql}: {name} primary key");
        }
    }
}

fn returning_names(items: &[SelectItem]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            SelectItem::Expr { alias: Some(a), .. } => a.clone(),
            SelectItem::Expr { expr: Expr::Column(parts), .. } => parts.last().cloned().unwrap_or_default(),
            other => panic!("unexpected RETURNING item {other:?}"),
        })
        .collect()
}

#[test]
fn parse_delete() {
    for (sql, want_where, want_returning) in [
        ("DELETE FROM t", false, vec![]),
        ("DELETE FROM t WHERE id = 1", true, vec![]),
        ("DELETE FROM t RETURNING id, name", false, vec!["id", "name"]),
        ("DELETE FROM t WHERE id = 1 RETURNING id", true, vec!["id"]),
    ] {
        let Statement::Delete(d) = stmt(sql) else { panic!("{sql}: not a DELETE") };
        assert_eq!(d.table.base(), "t");
        assert_eq!(d.selection.is_some(), want_where, "{sql}");
        assert_eq!(returning_names(&d.returning), want_returning, "{sql}");
    }
}

#[test]
fn parse_drop_table() {
    for (sql, if_exists) in [("DROP TABLE foo", false), ("DROP TABLE IF EXISTS foo", true)] {
        let Statement::Drop(d) = stmt(sql) else { panic!("{sql}: not a DROP") };
        assert_eq!(d.object_type, pgmem_ir::ObjectType::Table);
        assert_eq!(d.names[0].base(), "foo");
        assert_eq!(d.if_exists, if_exists, "{sql}");
    }
}

fn references(sql: &str, column: usize) -> pgmem_ir::ForeignKeyRef {
    let ct = create_table(sql);
    ct.columns[column]
        .constraints
        .iter()
        .find_map(|c| match &c.kind {
            ColumnConstraintKind::References(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{sql}: no REFERENCES"))
}

/// Without ON DELETE the action is NO ACTION (pgmem-go called it RESTRICT; the two differ
/// only in when the check runs, and the binder treats both as rejecting the delete).
#[test]
fn parse_on_delete_action() {
    for (sql, want) in [
        ("CREATE TABLE c (p int REFERENCES p(id))", ReferentialAction::NoAction),
        ("CREATE TABLE c (p int REFERENCES p(id) ON DELETE CASCADE)", ReferentialAction::Cascade),
        ("CREATE TABLE c (p int REFERENCES p(id) ON DELETE SET NULL)", ReferentialAction::SetNull),
        ("CREATE TABLE c (p int REFERENCES p(id) ON DELETE RESTRICT)", ReferentialAction::Restrict),
    ] {
        assert_eq!(references(sql, 0).on_delete, want, "{sql}");
    }
}

#[test]
fn parse_references_attaches_column_ref_spec() {
    for (sql, col) in [
        ("CREATE TABLE child (parent_id int REFERENCES parent(id))", 0),
        ("CREATE TABLE child (parent_id int NOT NULL REFERENCES parent(id))", 0),
        ("CREATE TABLE child (id int PRIMARY KEY, parent_id int REFERENCES parent(id))", 1),
    ] {
        let r = references(sql, col);
        assert_eq!(r.table.base(), "parent", "{sql}");
        assert_eq!(r.columns, ["id"], "{sql}");
    }
}

#[test]
fn parse_insert_returning() {
    for (sql, want) in [
        ("INSERT INTO t (id) VALUES (1)", vec![]),
        ("INSERT INTO t (id, name) VALUES (1, 'a') RETURNING id", vec!["id"]),
        ("INSERT INTO t (id, name) VALUES (1, 'a') RETURNING id, name", vec!["id", "name"]),
        ("INSERT INTO t (id) VALUES (1) RETURNING id AS new_id", vec!["new_id"]),
    ] {
        let Statement::Insert(i) = stmt(sql) else { panic!("{sql}: not an INSERT") };
        assert!(matches!(i.source, InsertSource::Values(_)), "{sql}");
        assert_eq!(returning_names(&i.returning), want, "{sql}");
    }
}

fn join_of(sql: &str) -> (JoinKind, JoinConstraint, TableRef, TableRef) {
    match select(sql).from.into_iter().next() {
        Some(TableRef::Join { left, right, kind, constraint, .. }) => (kind, constraint, *left, *right),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn parse_join_types() {
    for (sql, kind, has_on) in [
        ("SELECT a.id FROM a LEFT JOIN b ON a.id = b.a_id", JoinKind::Left, true),
        ("SELECT a.id FROM a LEFT OUTER JOIN b ON a.id = b.a_id", JoinKind::Left, true),
        ("SELECT a.id FROM a CROSS JOIN b", JoinKind::Cross, false),
    ] {
        let (k, c, _, _) = join_of(sql);
        assert_eq!(k, kind, "{sql}");
        assert_eq!(matches!(c, JoinConstraint::On(_)), has_on, "{sql}");
    }
}

#[test]
fn parse_cross_join_rejects_on() {
    let err = parse_one("SELECT a.id FROM a CROSS JOIN b ON a.id = b.a_id").expect_err("CROSS JOIN ... ON");
    assert_eq!(err.code, "42601");
    assert_eq!(err.message, "syntax error at or near \"ON\"");
}

#[test]
fn parse_join_builds_left_deep_tree() {
    for sql in ["SELECT a.id FROM a JOIN b ON a.id = b.a_id", "SELECT a.id FROM a INNER JOIN b ON a.id = b.a_id"] {
        let (kind, constraint, left, right) = join_of(sql);
        assert_eq!(kind, JoinKind::Inner, "{sql}");
        assert!(matches!(left, TableRef::Table { .. }), "{sql}");
        assert!(matches!(right, TableRef::Table { .. }), "{sql}");
        let JoinConstraint::On(cond) = constraint else { panic!("{sql}: no ON") };
        assert_eq!(binary(&cond).0, "=");
    }
}

#[test]
fn parse_qualified_column_ref() {
    assert_eq!(first_expr("SELECT a.id FROM a"), Expr::Column(vec!["a".into(), "id".into()]));
}

#[test]
fn parse_chained_joins() {
    let (_, _, inner, outer_right) = join_of("SELECT a.id FROM a JOIN b ON a.id = b.a_id JOIN c ON b.id = c.b_id");
    match outer_right {
        TableRef::Table { name, .. } => assert_eq!(name.base(), "c"),
        other => panic!("outer right: {other:?}"),
    }
    match inner {
        TableRef::Join { left, right, .. } => {
            assert!(matches!(*left, TableRef::Table { ref name, .. } if name.base() == "a"));
            assert!(matches!(*right, TableRef::Table { ref name, .. } if name.base() == "b"));
        }
        other => panic!("inner join shape: {other:?}"),
    }
}

/// SERIAL stays a type name in the parse tree; the binder turns it into an integer column
/// with NOT NULL and a sequence default (covered by the serial integration tests).
#[test]
fn parse_serial_desugars_to_int_plus_auto() {
    for (sql, ty) in [
        ("CREATE TABLE t (id serial)", "serial"),
        ("CREATE TABLE t (id bigserial)", "bigserial"),
        ("CREATE TABLE t (id serial PRIMARY KEY)", "serial"),
    ] {
        assert_eq!(create_table(sql).columns[0].ty.name, ty, "{sql}");
    }
}

#[test]
fn parse_scalar_subquery_in_select_list() {
    assert!(matches!(first_expr("SELECT (SELECT id FROM t WHERE k = 1) FROM other"), Expr::Subquery(_)));
}

#[test]
fn parse_in_list() {
    match select("SELECT id FROM t WHERE id IN (1, 2, 3)").selection {
        Some(Expr::InList { list, negated: false, .. }) => assert_eq!(list.len(), 3),
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_in_subquery() {
    assert!(matches!(
        select("SELECT id FROM t WHERE id IN (SELECT id FROM other)").selection,
        Some(Expr::InSubquery { negated: false, .. })
    ));
}

#[test]
fn parse_not_in() {
    match select("SELECT id FROM t WHERE id NOT IN (1, 2)").selection {
        Some(Expr::InList { list, negated: true, .. }) => assert_eq!(list.len(), 2),
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_update() {
    for (sql, assigns, want_where, want_returning) in [
        ("UPDATE t SET label = 'x'", vec!["label"], false, vec![]),
        ("UPDATE t SET label = 'x', qty = 5 WHERE id = 1", vec!["label", "qty"], true, vec![]),
        ("UPDATE t SET label = 'x' WHERE id = 1 RETURNING id, label", vec!["label"], true, vec!["id", "label"]),
    ] {
        let Statement::Update(u) = stmt(sql) else { panic!("{sql}: not an UPDATE") };
        let cols: Vec<&str> = u.assignments.iter().map(|a| a.columns[0][0].as_str()).collect();
        assert_eq!(cols, assigns, "{sql}");
        assert_eq!(u.selection.is_some(), want_where, "{sql}");
        assert_eq!(returning_names(&u.returning), want_returning, "{sql}");
    }
}
