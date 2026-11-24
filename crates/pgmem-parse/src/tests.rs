use super::*;
use pgmem_ir::{Expr, Literal, SelectItem, SetExpr};

fn one(sql: &str) -> Statement {
    parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn select_exprs(sql: &str) -> Vec<Expr> {
    match one(sql) {
        Statement::Query(q) => match q.body {
            SetExpr::Select(s) => s
                .projection
                .into_iter()
                .map(|i| match i {
                    SelectItem::Expr { expr, .. } => expr,
                    other => panic!("{other:?}"),
                })
                .collect(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn identifiers_fold_like_postgres() {
    let exprs = select_exprs(r#"SELECT Foo, "Bar", t.BAZ FROM t"#);
    assert_eq!(exprs[0], Expr::Column(vec!["foo".into()]));
    assert_eq!(exprs[1], Expr::Column(vec!["Bar".into()]));
    assert_eq!(exprs[2], Expr::Column(vec!["t".into(), "baz".into()]));
}

#[test]
fn negative_literals_fold() {
    let exprs = select_exprs("SELECT -2147483648, -1.5, $3");
    assert_eq!(exprs[0], Expr::Literal(Literal::Integer("-2147483648".into())));
    assert_eq!(exprs[1], Expr::Literal(Literal::Decimal("-1.5".into())));
    assert_eq!(exprs[2], Expr::Param(3));
}

#[test]
fn special_forms_become_functions() {
    let exprs = select_exprs("SELECT EXTRACT(year FROM now()), POSITION('b' IN 'abc'), TRIM(BOTH 'x' FROM 'xax'), CURRENT_TIMESTAMP");
    let names: Vec<String> = exprs
        .iter()
        .map(|e| match e {
            Expr::Function(f) => f.name.base().to_owned(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(names, ["extract", "position", "btrim", "current_timestamp"]);
}

#[test]
fn utility_statements() {
    assert!(matches!(one("BEGIN"), Statement::Transaction(TransactionStmt::Begin { .. })));
    assert!(matches!(
        one("START TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
        Statement::Transaction(TransactionStmt::Begin { isolation: Some(IsolationLevel::Serializable), .. })
    ));
    assert!(matches!(one("END"), Statement::Transaction(TransactionStmt::Commit { .. })));
    assert!(matches!(one("ABORT"), Statement::Transaction(TransactionStmt::Rollback { .. })));
    assert_eq!(one("ROLLBACK TO SAVEPOINT sp1"), Statement::Transaction(TransactionStmt::RollbackTo("sp1".into())));
    assert_eq!(one("COMMENT ON TABLE t IS 'x'"), Statement::NoOp { tag: "COMMENT".into() });
    assert_eq!(one("DISCARD ALL"), Statement::NoOp { tag: "DISCARD ALL".into() });
    assert!(matches!(one("SET search_path TO \"$user\", public"), Statement::Set { .. }));
    assert_eq!(one(""), Statement::Empty);
}

#[test]
fn syntax_errors_map_to_42601() {
    let err = parse("SELEC 1").expect_err("bad sql");
    assert_eq!(err.code, "42601");
    assert_eq!(err.message, "syntax error at or near \"SELEC\"");
    let err = parse_one("SELECT 1; SELECT 2").expect_err("two statements");
    assert_eq!(err.code, "42601");
}

#[test]
fn insert_with_default_marker() {
    match one("INSERT INTO t (a, b) VALUES (DEFAULT, 1) RETURNING *") {
        Statement::Insert(i) => match i.source {
            pgmem_ir::InsertSource::Values(rows) => assert_eq!(rows[0][0], Expr::Default),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn create_table_constraints() {
    match one(
        "CREATE TABLE t (id serial PRIMARY KEY, email text NOT NULL UNIQUE, parent int REFERENCES t(id) ON DELETE CASCADE, CHECK (id > 0))",
    ) {
        Statement::CreateTable(ct) => {
            assert_eq!(ct.columns.len(), 3);
            assert_eq!(ct.columns[0].ty.name, "serial");
            assert_eq!(ct.constraints.len(), 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn rewrites_postgres_only_lock_syntax() {
    for sql in [
        "SELECT * FROM jobs FOR NO KEY UPDATE",
        "SELECT * FROM jobs FOR KEY SHARE OF jobs NOWAIT",
        "SELECT 'FOR NO KEY UPDATE' FROM jobs FOR UPDATE SKIP LOCKED",
        "DROP INDEX CONCURRENTLY IF EXISTS idx",
    ] {
        parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // Literals are untouched by the rewrite.
    match one("SELECT 'FOR NO KEY UPDATE'") {
        Statement::Query(q) => match &q.body {
            SetExpr::Select(s) => match &s.projection[0] {
                SelectItem::Expr { expr: Expr::Literal(Literal::String(v)), .. } => assert_eq!(v, "FOR NO KEY UPDATE"),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
