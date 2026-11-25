//! `information_schema` views. Their domain types report as their base types
//! (`sql_identifier` → name, `cardinal_number` → int4, `character_data` / `yes_or_no` →
//! varchar, `time_stamp` → timestamptz), as they do in RowDescription.

use super::model::{self, Att, Con, Model, fk_action_rule};
use super::{Def, Spec, int4, name, snapshot, txt, yes_no};
use crate::eval::EvalCtx;
use crate::funcs::string::quote_ident;
use crate::plan::Row;
use pgmem_catalog::{ConstraintKind, Identity, TableDef, ViewDef};
use pgmem_types::{SqlResult, Type, Value};

const ID: &str = "name";
const CARD: &str = "int4";
const CHARS: &str = "varchar";

const SCHEMATA: &str = "catalog_name:name, schema_name:name, schema_owner:name, default_character_set_catalog:name, \
    default_character_set_schema:name, default_character_set_name:name, sql_path:varchar";
const TABLES: &str = "table_catalog:name, table_schema:name, table_name:name, table_type:varchar, self_referencing_column_name:name, \
    reference_generation:varchar, user_defined_type_catalog:name, user_defined_type_schema:name, user_defined_type_name:name, \
    is_insertable_into:varchar, is_typed:varchar=NO, commit_action:varchar";
const COLUMNS: &str = "table_catalog:name, table_schema:name, table_name:name, column_name:name, ordinal_position:int4, \
    column_default:varchar, is_nullable:varchar, data_type:varchar, character_maximum_length:int4, character_octet_length:int4, \
    numeric_precision:int4, numeric_precision_radix:int4, numeric_scale:int4, datetime_precision:int4, interval_type:varchar, \
    interval_precision:int4, character_set_catalog:name, character_set_schema:name, character_set_name:name, collation_catalog:name, \
    collation_schema:name, collation_name:name, domain_catalog:name, domain_schema:name, domain_name:name, udt_catalog:name, \
    udt_schema:name, udt_name:name, scope_catalog:name, scope_schema:name, scope_name:name, maximum_cardinality:int4, \
    dtd_identifier:name, is_self_referencing:varchar=NO, is_identity:varchar=NO, identity_generation:varchar, identity_start:varchar, \
    identity_increment:varchar, identity_maximum:varchar, identity_minimum:varchar, identity_cycle:varchar, \
    is_generated:varchar=NEVER, generation_expression:varchar, is_updatable:varchar";
const TABLE_CONSTRAINTS: &str = "constraint_catalog:name, constraint_schema:name, constraint_name:name, table_catalog:name, \
    table_schema:name, table_name:name, constraint_type:varchar, is_deferrable:varchar=NO, initially_deferred:varchar=NO, \
    enforced:varchar=YES, nulls_distinct:varchar";
const KEY_COLUMN_USAGE: &str = "constraint_catalog:name, constraint_schema:name, constraint_name:name, table_catalog:name, \
    table_schema:name, table_name:name, column_name:name, ordinal_position:int4, position_in_unique_constraint:int4";
const REFERENTIAL_CONSTRAINTS: &str = "constraint_catalog:name, constraint_schema:name, constraint_name:name, \
    unique_constraint_catalog:name, unique_constraint_schema:name, unique_constraint_name:name, match_option:varchar, \
    update_rule:varchar, delete_rule:varchar";
const CONSTRAINT_COLUMN_USAGE: &str = "table_catalog:name, table_schema:name, table_name:name, column_name:name, \
    constraint_catalog:name, constraint_schema:name, constraint_name:name";
const CHECK_CONSTRAINTS: &str = "constraint_catalog:name, constraint_schema:name, constraint_name:name, check_clause:varchar";
const VIEWS: &str = "table_catalog:name, table_schema:name, table_name:name, view_definition:varchar, check_option:varchar=NONE, \
    is_updatable:varchar, is_insertable_into:varchar, is_trigger_updatable:varchar=NO, is_trigger_deletable:varchar=NO, \
    is_trigger_insertable_into:varchar=NO";
const SEQUENCES: &str = "sequence_catalog:name, sequence_schema:name, sequence_name:name, data_type:varchar, numeric_precision:int4, \
    numeric_precision_radix:int4=2, numeric_scale:int4=0, start_value:varchar, minimum_value:varchar, maximum_value:varchar, \
    increment:varchar, cycle_option:varchar=NO";
const TABLE_PRIVILEGES: &str = "grantor:name, grantee:name, table_catalog:name, table_schema:name, table_name:name, \
    privilege_type:varchar, is_grantable:varchar, with_hierarchy:varchar";
const COLUMN_PRIVILEGES: &str = "grantor:name, grantee:name, table_catalog:name, table_schema:name, table_name:name, column_name:name, \
    privilege_type:varchar, is_grantable:varchar";
const ENABLED_ROLES: &str = "role_name:name";
const APPLICABLE_ROLES: &str = "grantee:name, role_name:name, is_grantable:varchar";
const ROUTINES: &str = "specific_catalog:name, specific_schema:name, specific_name:name, routine_catalog:name, routine_schema:name, \
    routine_name:name, routine_type:varchar, data_type:varchar, type_udt_name:name, routine_body:varchar, routine_definition:varchar, \
    external_language:varchar, is_deterministic:varchar, security_type:varchar";
const PARAMETERS: &str = "specific_catalog:name, specific_schema:name, specific_name:name, ordinal_position:int4, \
    parameter_mode:varchar, is_result:varchar, as_locator:varchar, parameter_name:name, data_type:varchar, udt_name:name, \
    parameter_default:varchar";
const TRIGGERS: &str = "trigger_catalog:name, trigger_schema:name, trigger_name:name, event_manipulation:varchar, \
    event_object_catalog:name, event_object_schema:name, event_object_table:name, action_order:int4, action_condition:varchar, \
    action_statement:varchar, action_orientation:varchar, action_timing:varchar, action_reference_old_table:name, \
    action_reference_new_table:name, action_reference_old_row:name, action_reference_new_row:name, created:timestamptz";
const DOMAINS: &str = "domain_catalog:name, domain_schema:name, domain_name:name, data_type:varchar, character_maximum_length:int4, \
    character_octet_length:int4, character_set_catalog:name, character_set_schema:name, character_set_name:name, \
    collation_catalog:name, collation_schema:name, collation_name:name, numeric_precision:int4, numeric_precision_radix:int4, \
    numeric_scale:int4, datetime_precision:int4, interval_type:varchar, interval_precision:int4, domain_default:varchar, \
    udt_catalog:name, udt_schema:name, udt_name:name, scope_catalog:name, scope_schema:name, scope_name:name, \
    maximum_cardinality:int4, dtd_identifier:name";
const DOMAIN_CONSTRAINTS: &str = "constraint_catalog:name, constraint_schema:name, constraint_name:name, domain_catalog:name, \
    domain_schema:name, domain_name:name, is_deferrable:varchar, initially_deferred:varchar";

macro_rules! def {
    ($name:literal, $oid:expr, $spec:expr, $rows:expr) => {
        Def { schema: "information_schema", name: $name, oid: $oid, kind: 'v', spec: $spec, rows: $rows }
    };
}

pub static DEFS: &[Def] = &[
    def!("schemata", 13_100, SCHEMATA, schemata),
    def!("tables", 13_101, TABLES, tables),
    def!("columns", 13_102, COLUMNS, columns),
    def!("table_constraints", 13_103, TABLE_CONSTRAINTS, table_constraints),
    def!("key_column_usage", 13_104, KEY_COLUMN_USAGE, key_column_usage),
    def!("referential_constraints", 13_105, REFERENTIAL_CONSTRAINTS, referential_constraints),
    def!("constraint_column_usage", 13_106, CONSTRAINT_COLUMN_USAGE, constraint_column_usage),
    def!("check_constraints", 13_107, CHECK_CONSTRAINTS, check_constraints),
    def!("views", 13_108, VIEWS, views),
    def!("sequences", 13_109, SEQUENCES, sequences),
    def!("table_privileges", 13_110, TABLE_PRIVILEGES, table_privileges),
    def!("role_table_grants", 13_111, TABLE_PRIVILEGES, table_privileges),
    def!("column_privileges", 13_112, COLUMN_PRIVILEGES, column_privileges),
    def!("enabled_roles", 13_113, ENABLED_ROLES, enabled_roles),
    def!("applicable_roles", 13_114, APPLICABLE_ROLES, empty),
    def!("routines", 13_115, ROUTINES, empty),
    def!("parameters", 13_116, PARAMETERS, empty),
    def!("triggers", 13_117, TRIGGERS, empty),
    def!("domains", 13_118, DOMAINS, domains),
    def!("domain_constraints", 13_119, DOMAIN_CONSTRAINTS, domain_constraints),
];

fn domains(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(DOMAINS);
    let cat = catalog_name(ctx);
    let mut rows = Vec::new();
    for t in m.catalog.types() {
        let pgmem_catalog::TypeDefKind::Domain { base, typmod, default, .. } = &t.kind else { continue };
        // A domain over a domain is USER-DEFINED (its immediate base is a user type).
        let mut vals = match base {
            Type::Domain(_) => {
                vec![("data_type", txt("USER-DEFINED")), ("udt_schema", name(&type_schema(base))), ("udt_name", name(&base.typname()))]
            }
            _ => type_columns(base, *typmod, &cat),
        };
        let default_text = default.as_ref().and_then(|d| {
            let empty =
                TableDef { oid: 0, schema: String::new(), name: String::new(), columns: vec![], constraints: vec![], indexes: vec![] };
            let mut col = pgmem_catalog::ColumnDef::new("value", base.clone());
            col.typmod = *typmod;
            col.default = Some(d.clone());
            crate::deparse::deparse_default(m.catalog, &empty, &col).ok().flatten()
        });
        vals.extend([
            ("domain_catalog", cat.clone()),
            ("domain_schema", name(&t.schema)),
            ("domain_name", name(&t.name)),
            ("domain_default", default_text.map_or(Value::Null, txt)),
            ("udt_catalog", cat.clone()),
            ("dtd_identifier", name("1")),
        ]);
        rows.push(s.row(vals));
    }
    Ok(rows)
}

fn domain_constraints(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(DOMAIN_CONSTRAINTS);
    let cat = catalog_name(ctx);
    let mut rows = Vec::new();
    for t in m.catalog.types() {
        for c in crate::catalog_views::domain_constraints(m.catalog, t) {
            rows.push(s.row(vec![
                ("constraint_catalog", cat.clone()),
                ("constraint_schema", name(&t.schema)),
                ("constraint_name", name(&c.name)),
                ("domain_catalog", cat.clone()),
                ("domain_schema", name(&t.schema)),
                ("domain_name", name(&t.name)),
                ("is_deferrable", txt("NO")),
                ("initially_deferred", txt("NO")),
            ]));
        }
    }
    Ok(rows)
}

/// The schema of a user type (or of an array's element type).
fn type_schema(t: &Type) -> String {
    match t {
        Type::Array(e) => type_schema(e),
        Type::Enum(o) | Type::Domain(o) | Type::Composite(o) => pgmem_types::usertype::schema_of(*o).unwrap_or_else(|| "public".to_owned()),
        _ => "pg_catalog".to_owned(),
    }
}

fn empty(_: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(Vec::new())
}

fn catalog_name(ctx: &EvalCtx) -> Value {
    name(&ctx.env.database)
}

fn schemata(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let s = Spec::parse(SCHEMATA);
    let _ = (ID, CARD, CHARS);
    let m = snapshot(ctx);
    let mut schemas: Vec<(String, String)> =
        ["information_schema", "pg_catalog", "pg_toast"].into_iter().map(|n| (n.to_owned(), ctx.env.user.clone())).collect();
    for sc in m.catalog.schemas() {
        let owner = if sc.name == "public" { "pg_database_owner".to_owned() } else { sc.owner.clone() };
        schemas.push((sc.name.clone(), owner));
    }
    Ok(schemas
        .into_iter()
        .map(|(schema, owner)| {
            s.row(vec![("catalog_name", catalog_name(ctx)), ("schema_name", name(&schema)), ("schema_owner", name(&owner))])
        })
        .collect())
}

/// A view Postgres considers automatically updatable: one table in FROM, plain column
/// outputs, no aggregation, DISTINCT, LIMIT or set operation.
fn view_updatable(v: &ViewDef) -> bool {
    let q = &v.query;
    if q.limit.is_some() || q.offset.is_some() || q.with.is_some() {
        return false;
    }
    let pgmem_ir::SetExpr::Select(sel) = &q.body else { return false };
    sel.from.len() == 1
        && matches!(sel.from[0], pgmem_ir::TableRef::Table { .. })
        && sel.group_by.is_empty()
        && sel.having.is_none()
        && sel.distinct == pgmem_ir::Distinct::All
        && sel.projection.iter().all(|p| match p {
            pgmem_ir::SelectItem::Expr { expr, .. } => matches!(expr, pgmem_ir::Expr::Column(_)),
            _ => true,
        })
}

fn tables(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(TABLES);
    let row = |schema: &str, n: &str, kind: &str, insertable: bool| {
        s.row(vec![
            ("table_catalog", catalog_name(ctx)),
            ("table_schema", name(schema)),
            ("table_name", name(n)),
            ("table_type", txt(kind)),
            ("is_insertable_into", yes_no(insertable)),
        ])
    };
    let mut rows: Vec<Row> = m
        .tables()
        .into_iter()
        .map(|t| row(&t.schema, &t.name, if t.schema.starts_with("pg_temp_") { "LOCAL TEMPORARY" } else { "BASE TABLE" }, true))
        .collect();
    rows.extend(m.views().into_iter().map(|v| row(&v.schema, &v.name, "VIEW", view_updatable(v))));
    Ok(rows)
}

/// The information_schema type columns of a column type: data_type, character lengths,
/// numeric precision/radix/scale and datetime precision.
fn type_columns(ty: &Type, typmod: i32, catalog: &Value) -> Vec<(&'static str, Value)> {
    // A domain column is described by its base type (with the domain's typmod) and names the
    // domain in the domain_* columns.
    if let Type::Domain(oid) = ty {
        let (base, dmod) = pgmem_types::usertype::domain_base(*oid).unwrap_or((Type::Unknown, -1));
        let mut out = type_columns(&base, if typmod >= 0 { typmod } else { dmod }, catalog);
        out.extend([("domain_catalog", catalog.clone()), ("domain_schema", name(&type_schema(ty))), ("domain_name", name(&ty.typname()))]);
        return out;
    }
    let user = |t: &Type| matches!(t, Type::Enum(_) | Type::Composite(_));
    let data_type = match ty {
        Type::Array(_) => "ARRAY".to_owned(),
        t if user(t) => "USER-DEFINED".to_owned(),
        other => other.name(),
    };
    let elem_user = matches!(ty, Type::Array(e) if user(e) || e.is_domain());
    let schema = match ty {
        t if user(t) => type_schema(t),
        Type::Array(e) if elem_user => type_schema(e),
        _ => "pg_catalog".to_owned(),
    };
    let mut out = vec![("data_type", txt(data_type)), ("udt_schema", name(&schema)), ("udt_name", name(&ty.typname()))];
    let null = || Value::Null;
    let n = |v: i64| int4(v);
    match ty {
        Type::Varchar | Type::Bpchar | Type::Text => {
            let len = (typmod >= 4).then(|| i64::from(typmod - 4));
            out.push(("character_maximum_length", len.map_or_else(null, n)));
            out.push(("character_octet_length", n(len.map_or(1_073_741_824, |l| l * 4))));
        }
        Type::Int2 | Type::Int4 | Type::Int8 => {
            let bits = match ty {
                Type::Int2 => 16,
                Type::Int4 => 32,
                _ => 64,
            };
            out.extend([("numeric_precision", n(bits)), ("numeric_precision_radix", n(2)), ("numeric_scale", n(0))]);
        }
        Type::Float4 | Type::Float8 => {
            out.extend([("numeric_precision", n(if *ty == Type::Float4 { 24 } else { 53 })), ("numeric_precision_radix", n(2))]);
        }
        Type::Numeric => {
            out.push(("numeric_precision_radix", n(10)));
            if typmod >= 4 {
                let (p, s) = crate::cast::numeric_typmod_parts(typmod);
                out.extend([("numeric_precision", n(i64::from(p))), ("numeric_scale", n(i64::from(s)))]);
            }
        }
        Type::Date => out.push(("datetime_precision", n(0))),
        Type::Time | Type::TimeTz | Type::Timestamp | Type::Timestamptz | Type::Interval => {
            out.push(("datetime_precision", n(if typmod >= 0 { i64::from(typmod) } else { 6 })));
        }
        _ => {}
    }
    out
}

/// The sequence behind an identity column.
fn identity_sequence<'a>(m: &Model<'a>, t: &TableDef, column: &str) -> Option<&'a pgmem_catalog::SequenceDef> {
    m.sequences().into_iter().find(|s| s.schema == t.schema && s.owned_by.as_ref().is_some_and(|(tn, cn)| *tn == t.name && cn == column))
}

#[allow(clippy::too_many_arguments)]
fn column_row(ctx: &EvalCtx, m: &Model, s: &Spec, schema: &str, rel: &str, table: Option<&TableDef>, a: &Att, updatable: bool) -> Row {
    let mut vals = vec![
        ("table_catalog", catalog_name(ctx)),
        ("table_schema", name(schema)),
        ("table_name", name(rel)),
        ("column_name", name(&a.name)),
        ("ordinal_position", int4(i64::try_from(a.attnum).unwrap_or(0))),
        ("is_nullable", yes_no(!a.not_null)),
        ("udt_catalog", catalog_name(ctx)),
        ("dtd_identifier", name(&a.attnum.to_string())),
        ("is_updatable", yes_no(updatable)),
    ];
    vals.extend(type_columns(&a.ty, a.typmod, &catalog_name(ctx)));
    if let (Some(t), Some(c)) = (table, a.column) {
        if c.generated.is_none()
            && let Ok(Some(d)) = crate::deparse::deparse_default(m.catalog, t, c)
        {
            vals.push(("column_default", txt(d)));
        }
        if c.generated.is_some() {
            vals.push(("is_generated", txt("ALWAYS")));
            let g = crate::deparse::deparse_generated(m.catalog, t, c).ok().flatten();
            vals.push(("generation_expression", g.map_or(Value::Null, txt)));
        }
        if let Some(id) = c.identity {
            vals.push(("is_identity", txt("YES")));
            vals.push(("identity_generation", txt(if id == Identity::Always { "ALWAYS" } else { "BY DEFAULT" })));
            if let Some(seq) = identity_sequence(m, t, &c.name) {
                vals.extend([
                    ("identity_start", txt(seq.start.to_string())),
                    ("identity_increment", txt(seq.increment.to_string())),
                    ("identity_maximum", txt(seq.max.to_string())),
                    ("identity_minimum", txt(seq.min.to_string())),
                    ("identity_cycle", txt("NO")),
                ]);
            }
        }
    }
    s.row(vals)
}

fn columns(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(COLUMNS);
    let mut rows = Vec::new();
    for t in m.tables() {
        for a in Model::table_atts(t).iter().filter(|a| !a.dropped) {
            rows.push(column_row(ctx, &m, &s, &t.schema, &t.name, Some(t), a, true));
        }
    }
    for v in m.views() {
        let updatable = view_updatable(v);
        for a in &m.view_atts(v) {
            rows.push(column_row(ctx, &m, &s, &v.schema, &v.name, None, a, updatable));
        }
    }
    Ok(rows)
}

fn constraint_type(c: &Con) -> &'static str {
    match c.contype {
        'p' => "PRIMARY KEY",
        'u' => "UNIQUE",
        'f' => "FOREIGN KEY",
        _ => "CHECK",
    }
}

fn table_constraints(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(TABLE_CONSTRAINTS);
    Ok(m.constraints()
        .iter()
        .map(|c| {
            let nulls_distinct = match c.kind {
                Some(ConstraintKind::Unique { nulls_not_distinct, .. }) => yes_no(!nulls_not_distinct),
                _ => Value::Null,
            };
            s.row(vec![
                ("constraint_catalog", catalog_name(ctx)),
                ("constraint_schema", name(&c.table.schema)),
                ("constraint_name", name(&c.name)),
                ("table_catalog", catalog_name(ctx)),
                ("table_schema", name(&c.table.schema)),
                ("table_name", name(&c.table.name)),
                ("constraint_type", txt(constraint_type(c))),
                ("is_deferrable", yes_no(c.flags().0)),
                ("initially_deferred", yes_no(c.flags().1)),
                ("enforced", txt("YES")),
                ("nulls_distinct", nulls_distinct),
            ])
        })
        .collect())
}

fn column_name(t: &TableDef, attnum: usize) -> String {
    t.columns.get(attnum.saturating_sub(1)).map(|c| c.name.clone()).unwrap_or_default()
}

/// The PRIMARY KEY / UNIQUE constraint a foreign key references.
fn referenced_constraint<'a>(all: &'a [Con<'a>], fk: &Con) -> Option<&'a Con<'a>> {
    all.iter().find(|c| matches!(c.contype, 'p' | 'u') && c.table.oid == fk.confrelid && c.conindid == fk.conindid)
}

fn key_column_usage(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(KEY_COLUMN_USAGE);
    let all = m.constraints();
    let mut rows = Vec::new();
    for c in all.iter().filter(|c| matches!(c.contype, 'p' | 'u' | 'f')) {
        let target = (c.contype == 'f').then(|| referenced_constraint(&all, c)).flatten();
        for (i, attnum) in c.conkey.iter().enumerate() {
            let position = target.and_then(|t| {
                let referenced = c.confkey.get(i)?;
                t.conkey.iter().position(|k| k == referenced).map(|p| int4(i64::try_from(p + 1).unwrap_or(0)))
            });
            rows.push(s.row(vec![
                ("constraint_catalog", catalog_name(ctx)),
                ("constraint_schema", name(&c.table.schema)),
                ("constraint_name", name(&c.name)),
                ("table_catalog", catalog_name(ctx)),
                ("table_schema", name(&c.table.schema)),
                ("table_name", name(&c.table.name)),
                ("column_name", name(&column_name(c.table, *attnum))),
                ("ordinal_position", int4(i64::try_from(i + 1).unwrap_or(0))),
                ("position_in_unique_constraint", position.unwrap_or(Value::Null)),
            ]));
        }
    }
    Ok(rows)
}

fn referential_constraints(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(REFERENTIAL_CONSTRAINTS);
    let all = m.constraints();
    Ok(all
        .iter()
        .filter_map(|c| match c.kind {
            Some(ConstraintKind::ForeignKey { on_delete, on_update, match_full, .. }) => Some((c, *on_delete, *on_update, *match_full)),
            _ => None,
        })
        .map(|(c, on_delete, on_update, match_full)| {
            let unique = referenced_constraint(&all, c);
            s.row(vec![
                ("constraint_catalog", catalog_name(ctx)),
                ("constraint_schema", name(&c.table.schema)),
                ("constraint_name", name(&c.name)),
                ("unique_constraint_catalog", unique.map_or(Value::Null, |_| catalog_name(ctx))),
                ("unique_constraint_schema", unique.map_or(Value::Null, |u| name(&u.table.schema))),
                ("unique_constraint_name", unique.map_or(Value::Null, |u| name(&u.name))),
                ("match_option", txt(if match_full { "FULL" } else { "NONE" })),
                ("update_rule", txt(fk_action_rule(on_update))),
                ("delete_rule", txt(fk_action_rule(on_delete))),
            ])
        })
        .collect())
}

fn constraint_column_usage(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(CONSTRAINT_COLUMN_USAGE);
    let mut rows = Vec::new();
    for c in &m.constraints() {
        // A foreign key "uses" the referenced columns.
        let (table, attnums) = if c.contype == 'f' {
            match m.catalog.tables().find(|t| t.oid == c.confrelid) {
                Some(t) => (t.as_ref(), &c.confkey),
                None => continue,
            }
        } else {
            (c.table, &c.conkey)
        };
        for attnum in attnums {
            rows.push(s.row(vec![
                ("table_catalog", catalog_name(ctx)),
                ("table_schema", name(&table.schema)),
                ("table_name", name(&table.name)),
                ("column_name", name(&column_name(table, *attnum))),
                ("constraint_catalog", catalog_name(ctx)),
                ("constraint_schema", name(&c.table.schema)),
                ("constraint_name", name(&c.name)),
            ]));
        }
    }
    Ok(rows)
}

fn check_constraints(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(CHECK_CONSTRAINTS);
    Ok(m.constraints()
        .iter()
        .filter(|c| matches!(c.contype, 'c' | 'n'))
        .map(|c| {
            let clause = if c.contype == 'n' {
                format!("{} IS NOT NULL", quote_ident(&column_name(c.table, c.conkey.first().copied().unwrap_or(0))))
            } else if let Some(ConstraintKind::Check { expr }) = c.kind {
                crate::deparse::deparse_predicate(m.catalog, c.table, expr).unwrap_or_default()
            } else {
                String::new()
            };
            s.row(vec![
                ("constraint_catalog", catalog_name(ctx)),
                ("constraint_schema", name(&c.table.schema)),
                ("constraint_name", name(&c.name)),
                ("check_clause", txt(clause)),
            ])
        })
        .chain(m.catalog.types().flat_map(|t| crate::catalog_views::domain_constraints(m.catalog, t).into_iter().map(move |c| (t, c))).map(
            |(t, c)| {
                s.row(vec![
                    ("constraint_catalog", catalog_name(ctx)),
                    ("constraint_schema", name(&t.schema)),
                    ("constraint_name", name(&c.name)),
                    ("check_clause", txt(c.check.unwrap_or_else(|| "VALUE IS NOT NULL".to_owned()))),
                ])
            },
        ))
        .collect())
}

fn views(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(VIEWS);
    Ok(m.views()
        .into_iter()
        .map(|v| {
            let updatable = view_updatable(v);
            s.row(vec![
                ("table_catalog", catalog_name(ctx)),
                ("table_schema", name(&v.schema)),
                ("table_name", name(&v.name)),
                ("view_definition", txt(crate::deparse::deparse_view(m.catalog, v).unwrap_or_default())),
                ("is_updatable", yes_no(updatable)),
                ("is_insertable_into", yes_no(updatable)),
            ])
        })
        .collect())
}

fn sequences(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(SEQUENCES);
    // Identity columns' sequences are internal and not listed.
    let identity = |seq: &pgmem_catalog::SequenceDef| {
        seq.owned_by
            .as_ref()
            .is_some_and(|(t, c)| m.catalog.table(&seq.schema, t).and_then(|t| t.column(c)).is_some_and(|c| c.identity.is_some()))
    };
    Ok(m.sequences()
        .into_iter()
        .filter(|seq| !identity(seq))
        .map(|seq| {
            let ty = model::sequence_type(seq);
            let bits = match ty {
                Type::Int2 => 16,
                Type::Int4 => 32,
                _ => 64,
            };
            s.row(vec![
                ("sequence_catalog", catalog_name(ctx)),
                ("sequence_schema", name(&seq.schema)),
                ("sequence_name", name(&seq.name)),
                ("data_type", txt(ty.name())),
                ("numeric_precision", int4(bits)),
                ("start_value", txt(seq.start.to_string())),
                ("minimum_value", txt(seq.min.to_string())),
                ("maximum_value", txt(seq.max.to_string())),
                ("increment", txt(seq.increment.to_string())),
            ])
        })
        .collect())
}

fn table_privileges(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(TABLE_PRIVILEGES);
    let user = &ctx.env.user;
    let rels: Vec<(String, String)> = m
        .tables()
        .into_iter()
        .map(|t| (t.schema.clone(), t.name.clone()))
        .chain(m.views().into_iter().map(|v| (v.schema.clone(), v.name.clone())))
        .collect();
    let mut rows = Vec::new();
    for (schema, rel) in rels {
        for p in ["INSERT", "SELECT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"] {
            rows.push(s.row(vec![
                ("grantor", name(user)),
                ("grantee", name(user)),
                ("table_catalog", catalog_name(ctx)),
                ("table_schema", name(&schema)),
                ("table_name", name(&rel)),
                ("privilege_type", txt(p)),
                ("is_grantable", yes_no(true)),
                ("with_hierarchy", yes_no(p == "SELECT")),
            ]));
        }
    }
    Ok(rows)
}

fn column_privileges(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(COLUMN_PRIVILEGES);
    let user = &ctx.env.user;
    let mut rows = Vec::new();
    for t in m.tables() {
        for (_, c) in t.live_columns() {
            for p in ["INSERT", "SELECT", "UPDATE", "REFERENCES"] {
                rows.push(s.row(vec![
                    ("grantor", name(user)),
                    ("grantee", name(user)),
                    ("table_catalog", catalog_name(ctx)),
                    ("table_schema", name(&t.schema)),
                    ("table_name", name(&t.name)),
                    ("column_name", name(&c.name)),
                    ("privilege_type", txt(p)),
                    ("is_grantable", yes_no(true)),
                ]));
            }
        }
    }
    Ok(rows)
}

fn enabled_roles(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(super::rows_of(ENABLED_ROLES, |s| vec![s.row(vec![("role_name", name(&ctx.env.user))])]))
}
