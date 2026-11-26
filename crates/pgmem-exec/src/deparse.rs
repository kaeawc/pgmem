//! The deparser: turns stored expressions and view queries back into SQL text exactly as
//! PostgreSQL 18's `ruleutils.c` prints them, for `pg_get_expr`, `pg_get_constraintdef`,
//! `pg_get_indexdef`, `pg_get_viewdef` and the `information_schema` columns built on them.
//!
//! pgmem keeps defaults, CHECK constraints, index expressions and view queries as unbound IR.
//! Postgres stores them after parse analysis, and ruleutils prints the casts that analysis
//! inserted (`price >= 0` on a numeric column reads back as `(price >= (0)::numeric)`). So the
//! deparser first re-runs a model of parse analysis over the IR (`node`: operator and
//! function resolution following Postgres' catalog, implicit coercions, `IN` lists folded into
//! `= ANY (ARRAY[...])`, BETWEEN expanded, ...) and then prints the result with a transcription
//! of ruleutils (`print`) in its `PRETTYFLAG_INDENT` mode, which every one of those
//! functions uses: full parenthesization, line breaks and indentation for CASE and queries.

mod node;
pub(crate) mod pg;
mod print;
mod query;
#[cfg(test)]
mod tests;

use node::{Analyzer, Level, Node, Rte, ScopeCol, coerce};
use pgmem_catalog::{Catalog, ColumnDef, Constraint, ConstraintKind, FkAction, IndexDef, TableDef, ViewDef};
use pgmem_ir as ir;
use pgmem_types::SqlResult;
pub use print::with_pretty;
use print::{Printer, expr_text, looks_like_function};

pub use pg::{format_type, quote_ident};

/// An analyzer whose only scope is `table` (unqualified column references, like
/// `deparse_context_for(relname, relid)`).
fn table_analyzer<'a>(catalog: &'a Catalog, table: &TableDef) -> Analyzer<'a> {
    let cols =
        table.live_columns().map(|(_, c)| ScopeCol { name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod, hidden: false }).collect();
    let rte = Rte { qualifier: table.name.clone(), refname: table.name.clone(), cols };
    Analyzer { catalog, levels: vec![Level { rtes: vec![rte], names: vec![table.name.clone()], ..Level::default() }], display_path: None }
}

/// An expression stored on `table` (an index expression or predicate, a CHECK expression),
/// printed as `pg_get_expr(expr, table)` does: `(price >= (0)::numeric)`.
pub fn deparse_expr(catalog: &Catalog, table: &TableDef, e: &ir::Expr) -> SqlResult<String> {
    let mut a = table_analyzer(catalog, table);
    let n = a.expr(e);
    Ok(expr_text(&n))
}

/// A boolean expression stored on `table` (CHECK constraints, partial-index predicates):
/// like [`deparse_expr`], with untyped literals resolved as boolean. This is
/// `information_schema.check_constraints.check_clause`.
pub fn deparse_predicate(catalog: &Catalog, table: &TableDef, e: &ir::Expr) -> SqlResult<String> {
    let mut a = table_analyzer(catalog, table);
    let n = a.boolean(e);
    Ok(expr_text(&n))
}

/// The range-table name standing for a domain constraint's `VALUE`.
const DOMAIN_VALUE_REF: &str = "\0domain value";

/// A domain CHECK expression as `pg_get_constraintdef` prints it inside `CHECK (...)`:
/// `(VALUE > 0)`.
pub fn deparse_domain_check(catalog: &Catalog, base: &pgmem_types::Type, typmod: i32, e: &ir::Expr) -> SqlResult<String> {
    let cols = vec![ScopeCol { name: "value".into(), ty: base.clone(), typmod, hidden: false }];
    let rte = Rte { qualifier: String::new(), refname: DOMAIN_VALUE_REF.into(), cols };
    let mut a = Analyzer { catalog, levels: vec![Level { rtes: vec![rte], names: vec![], ..Level::default() }], display_path: None };
    let n = a.boolean(e);
    Ok(expr_text(&n))
}

/// The expression coerced to the column's type, as `cookDefault` stores it.
fn column_expr(catalog: &Catalog, table: &TableDef, column: &ColumnDef, e: &ir::Expr) -> Node {
    let mut a = table_analyzer(catalog, table);
    let n = a.expr(e);
    coerce(n, &column.ty, column.typmod, false)
}

/// A column default as `pg_get_expr(adbin, adrelid)` / `information_schema.columns
/// .column_default` print it (`nextval('users_id_seq'::regclass)`, `'member'::character
/// varying`, `CURRENT_TIMESTAMP`). `None` when Postgres stores no default: no DEFAULT clause,
/// a NULL default (Postgres drops those), or an identity column (its sequence is not a default).
pub fn deparse_default(catalog: &Catalog, table: &TableDef, column: &ColumnDef) -> SqlResult<Option<String>> {
    let Some(e) = &column.default else { return Ok(None) };
    if column.identity.is_some() {
        return Ok(None);
    }
    let n = column_expr(catalog, table, column, e);
    if matches!(n, Node::Const { value: None, .. }) {
        return Ok(None);
    }
    Ok(Some(expr_text(&n)))
}

/// The generation expression of a `GENERATED ALWAYS AS (...) STORED` column, as
/// `pg_get_expr(adbin, adrelid)` and `information_schema.columns.generation_expression` print it.
pub fn deparse_generated(catalog: &Catalog, table: &TableDef, column: &ColumnDef) -> SqlResult<Option<String>> {
    let Some(e) = &column.generated else { return Ok(None) };
    Ok(Some(expr_text(&column_expr(catalog, table, column, e))))
}

fn column_list(table: &TableDef, columns: &[usize]) -> String {
    columns.iter().map(|&i| table.columns.get(i).map_or_else(String::new, |c| quote_ident(&c.name))).collect::<Vec<_>>().join(", ")
}

fn fk_action(a: FkAction) -> &'static str {
    match a {
        FkAction::NoAction => "NO ACTION",
        FkAction::Restrict => "RESTRICT",
        FkAction::Cascade => "CASCADE",
        FkAction::SetNull => "SET NULL",
        FkAction::SetDefault => "SET DEFAULT",
    }
}

/// A constraint as `pg_get_constraintdef(oid)` prints it: `CHECK ((price > (0)::numeric))`,
/// `PRIMARY KEY (id)`, `UNIQUE (org_id, name)`, `FOREIGN KEY (user_id) REFERENCES users(id)
/// ON DELETE CASCADE`.
pub fn deparse_constraint(catalog: &Catalog, table: &TableDef, constraint: &Constraint) -> SqlResult<String> {
    Ok(match &constraint.kind {
        ConstraintKind::Check { expr } => format!("CHECK ({})", deparse_predicate(catalog, table, expr)?),
        ConstraintKind::PrimaryKey { columns } => format!("PRIMARY KEY ({})", column_list(table, columns)),
        ConstraintKind::Unique { columns, nulls_not_distinct } => {
            format!("UNIQUE {}({})", if *nulls_not_distinct { "NULLS NOT DISTINCT " } else { "" }, column_list(table, columns))
        }
        ConstraintKind::Exclude { method, elements, predicate, temporal } => {
            let key = |e: &ir::Expr| -> SqlResult<String> {
                let mut a = table_analyzer(catalog, table);
                let n = a.expr(e);
                let text = expr_text(&n);
                Ok(if looks_like_function(&n) || matches!(n, Node::Var { .. }) { text } else { format!("({text})") })
            };
            if let Some(primary) = temporal {
                let cols = elements.iter().map(|(e, _)| key(e)).collect::<SqlResult<Vec<_>>>()?;
                let n = cols.len();
                let list: Vec<String> =
                    cols.into_iter().enumerate().map(|(i, c)| if i + 1 == n { format!("{c} WITHOUT OVERLAPS") } else { c }).collect();
                return Ok(format!("{} ({})", if *primary { "PRIMARY KEY" } else { "UNIQUE" }, list.join(", ")));
            }
            let items = elements.iter().map(|(e, op)| Ok(format!("{} WITH {op}", key(e)?))).collect::<SqlResult<Vec<_>>>()?;
            let mut s = format!("EXCLUDE USING {method} ({})", items.join(", "));
            if let Some(p) = predicate {
                s.push_str(&format!(" WHERE ({})", deparse_predicate(catalog, table, p)?));
            }
            s
        }
        ConstraintKind::ForeignKey { columns, ref_table, ref_columns, on_delete, on_update, match_full, delete_set_columns } => {
            let ref_cols = match catalog.table_q(ref_table) {
                Some(t) if *ref_table != table.qualified() => column_list(t, ref_columns),
                _ => column_list(table, ref_columns),
            };
            let target = relation_name(catalog, &ref_table.schema, &ref_table.name);
            let mut s = format!("FOREIGN KEY ({}) REFERENCES {target}({ref_cols})", column_list(table, columns));
            if *match_full {
                s.push_str(" MATCH FULL");
            }
            if *on_update != FkAction::NoAction {
                s.push_str(&format!(" ON UPDATE {}", fk_action(*on_update)));
            }
            if *on_delete != FkAction::NoAction {
                s.push_str(&format!(" ON DELETE {}", fk_action(*on_delete)));
                if !delete_set_columns.is_empty() {
                    s.push_str(&format!(" ({})", column_list(table, delete_set_columns)));
                }
            }
            s
        }
    })
}

/// An index as `pg_get_indexdef(oid)` prints it:
/// `CREATE UNIQUE INDEX users_email_key ON public.users USING btree (lower((email)::text)) WHERE (deleted_at IS NULL)`.
pub fn deparse_index(catalog: &Catalog, table: &TableDef, index: &IndexDef) -> SqlResult<String> {
    let mut keys: Vec<String> = index.columns.iter().map(|&i| column_list(table, &[i])).collect();
    for e in &index.exprs {
        let mut a = table_analyzer(catalog, table);
        let n = a.expr(e);
        let text = expr_text(&n);
        keys.push(if looks_like_function(&n) || matches!(n, Node::Var { .. }) { text } else { format!("({text})") });
    }
    for (key, oc) in keys.iter_mut().zip(&index.opclasses) {
        if let Some(name) = &oc.shown {
            key.push(' ');
            key.push_str(&quote_ident(name));
        }
    }
    let mut s = format!(
        "CREATE {}INDEX {} ON {}.{} USING {} ({})",
        if index.unique { "UNIQUE " } else { "" },
        quote_ident(&index.name),
        quote_ident(&table.schema),
        quote_ident(&table.name),
        index.method,
        keys.join(", ")
    );
    if !index.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", index.include.iter().map(|&i| column_list(table, &[i])).collect::<Vec<_>>().join(", ")));
    }
    if index.nulls_not_distinct {
        s.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(p) = &index.predicate {
        s.push_str(&format!(" WHERE {}", deparse_predicate(catalog, table, p)?));
    }
    Ok(s)
}

/// A view's query as `pg_get_viewdef(oid)` / `pg_views.definition` /
/// `information_schema.views.view_definition` print it: pretty format, with the leading
/// space and the trailing semicolon (`" SELECT id,\n    title\n   FROM books;"`).
/// ruleutils' `generate_relation_name`: the name alone when the search path finds this
/// relation by it, else schema-qualified.
pub fn relation_name(catalog: &Catalog, schema: &str, name: &str) -> String {
    if catalog.relation_visible(schema, name) { quote_ident(name) } else { format!("{}.{}", quote_ident(schema), quote_ident(name)) }
}

pub fn deparse_view(catalog: &Catalog, view: &ViewDef) -> SqlResult<String> {
    let display_path = Some(pgmem_types::context::search_path().entries);
    let mut a = Analyzer { catalog, levels: Vec::new(), display_path };
    let q = pgmem_types::context::with_search_path(&view.search_path, || a.query(&view.query, false));
    let mut p = Printer::new(q.varprefix);
    p.query_def(&q, Some(&view.columns), true, 0);
    p.buf.push(';');
    Ok(p.buf)
}
