//! pgmem's catalog in Postgres' terms: relation OIDs and kinds, attribute numbers, the
//! constraints Postgres 18 records (including one NOT NULL constraint per non-null column),
//! index keys, and the definition strings `pg_get_constraintdef` / `pg_get_indexdef` print.

use crate::bind::{Binder, Ctx, Level, Scope};
use crate::funcs::string::quote_ident;
use crate::session::SessionEnv;
use pgmem_catalog::{Catalog, ColumnDef, ConstraintKind, FkAction, Identity, IndexDef, Oid, SequenceDef, TableDef, ViewDef};
use pgmem_types::Type;

/// `pg_class` OIDs of the catalogs objects are described by (`classoid` columns).
pub const CLASS_CLASS_OID: Oid = 1259;
pub const ATTRDEF_CLASS_OID: Oid = 2604;
pub const CONSTRAINT_CLASS_OID: Oid = 2606;
pub const NAMESPACE_CLASS_OID: Oid = 2615;
pub const PROC_CLASS_OID: Oid = 1255;
pub const TRIGGER_CLASS_OID: Oid = 2620;
/// information_schema's namespace OID differs between Postgres builds; any fixed value works.
pub const INFORMATION_SCHEMA_NAMESPACE_OID: Oid = 13_000;
/// The bootstrap superuser every pgmem session runs as.
pub const SUPERUSER_OID: Oid = 10;
pub const PG_DATABASE_OWNER_OID: Oid = 6171;
pub const DEFAULT_COLLATION_OID: Oid = 100;
pub const C_COLLATION_OID: Oid = 950;

// Objects pgmem doesn't store separately get OIDs derived from their table's OID.
const NOT_NULL_OID_BASE: Oid = 0x4000_0000;
const ATTRDEF_OID_BASE: Oid = 0x5000_0000;
const ROWTYPE_OID_BASE: Oid = 0x6000_0000;
const PER_TABLE: Oid = 1700;

pub fn not_null_oid(table: Oid, attnum: usize) -> Oid {
    NOT_NULL_OID_BASE.wrapping_add(table.wrapping_mul(PER_TABLE)).wrapping_add(u32::try_from(attnum).unwrap_or(0))
}

pub fn attrdef_oid(table: Oid, attnum: usize) -> Oid {
    ATTRDEF_OID_BASE.wrapping_add(table.wrapping_mul(PER_TABLE)).wrapping_add(u32::try_from(attnum).unwrap_or(0))
}

/// The OID of a table's composite row type.
pub fn rowtype_oid(table: Oid) -> Oid {
    ROWTYPE_OID_BASE.wrapping_add(table)
}

/// Postgres 18 names a column's NOT NULL constraint `<table>_<column>_not_null`.
pub fn not_null_constraint_name(table: &str, column: &str) -> String {
    pgmem_catalog::default_constraint_name(table, &[column], "not_null")
}

/// The data type a sequence was declared with, recovered from its range.
pub fn sequence_type(seq: &SequenceDef) -> Type {
    seq.data_type.clone()
}

/// One attribute of a relation (`pg_attribute`), user columns only (attnum > 0).
pub struct Att<'a> {
    pub attnum: usize,
    pub name: String,
    pub ty: Type,
    pub typmod: i32,
    pub not_null: bool,
    pub dropped: bool,
    /// The column definition, for table columns.
    pub column: Option<&'a ColumnDef>,
}

/// A constraint as `pg_constraint` records it.
pub struct Con<'a> {
    pub oid: Oid,
    pub name: String,
    pub contype: char,
    pub table: &'a TableDef,
    /// Attribute numbers of the constrained columns.
    pub conkey: Vec<usize>,
    pub confrelid: Oid,
    pub confkey: Vec<usize>,
    /// The index enforcing (or, for a foreign key, referenced by) the constraint.
    pub conindid: Oid,
    pub on_update: char,
    pub on_delete: char,
    pub kind: Option<&'a ConstraintKind>,
}

impl Con<'_> {
    /// `(condeferrable, condeferred, convalidated)` of the catalog constraint (NOT NULL
    /// constraints: not deferrable, valid).
    pub fn flags(&self) -> (bool, bool, bool) {
        self.table
            .constraints
            .iter()
            .find(|c| c.oid == self.oid && self.kind.is_some())
            .map_or((false, false, true), |c| (c.deferrable, c.initially_deferred, c.validated))
    }
}

pub struct Model<'a> {
    pub catalog: &'a Catalog,
    pub env: &'a SessionEnv,
}

impl<'a> Model<'a> {
    pub fn new(catalog: &'a Catalog, env: &'a SessionEnv) -> Model<'a> {
        Model { catalog, env }
    }

    pub fn tables(&self) -> Vec<&'a TableDef> {
        self.catalog.tables().map(AsRef::as_ref).collect()
    }

    pub fn views(&self) -> Vec<&'a ViewDef> {
        self.catalog.views().map(AsRef::as_ref).collect()
    }

    pub fn sequences(&self) -> Vec<&'a SequenceDef> {
        self.catalog.sequences().map(AsRef::as_ref).collect()
    }

    /// The namespace OID of a schema name.
    pub fn namespace(&self, schema: &str) -> u32 {
        self.catalog.namespace_oid(schema).unwrap_or(pgmem_catalog::PUBLIC_NAMESPACE_OID)
    }

    /// Every index with its table.
    pub fn indexes(&self) -> Vec<(&'a TableDef, &'a IndexDef)> {
        self.catalog.tables().flat_map(|t| t.indexes.iter().map(move |i| (t.as_ref(), i))).collect()
    }

    /// A table's attributes, dropped slots included (Postgres keeps them with
    /// `attisdropped`).
    pub fn table_atts(t: &'a TableDef) -> Vec<Att<'a>> {
        t.columns
            .iter()
            .enumerate()
            .map(|(i, c)| Att {
                attnum: i + 1,
                name: if c.dropped { format!("........pg.dropped.{}........", i + 1) } else { c.name.clone() },
                ty: c.ty.clone(),
                typmod: c.typmod,
                not_null: c.not_null && !c.dropped,
                dropped: c.dropped,
                column: Some(c),
            })
            .collect()
    }

    /// A view's output columns, typed by binding its query against the current catalog.
    pub fn view_atts(&self, v: &ViewDef) -> Vec<Att<'a>> {
        let binder = Binder::new(self.catalog, &[]);
        let top = Level::new(Scope::default());
        let ctx = Ctx::new(&top);
        let cols = binder.bind_query(&v.query, &ctx).map(|b| b.columns).unwrap_or_default();
        v.columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let (ty, typmod) = cols.get(i).map_or((Type::Text, -1), |c| (c.ty.clone(), c.typmod));
                Att { attnum: i + 1, name: name.clone(), ty, typmod, not_null: false, dropped: false, column: None }
            })
            .collect()
    }

    /// The columns of an index (`pg_attribute` rows of the index relation).
    pub fn index_atts(t: &TableDef, idx: &IndexDef) -> Vec<Att<'a>> {
        let mut out: Vec<Att> = idx
            .columns
            .iter()
            .filter_map(|&c| t.columns.get(c))
            .map(|c| Att {
                attnum: 0,
                name: c.name.clone(),
                ty: c.ty.clone(),
                typmod: c.typmod,
                not_null: false,
                dropped: false,
                column: None,
            })
            .collect();
        for e in &idx.exprs {
            let name = match e {
                pgmem_ir::Expr::Function(f) => f.name.base().to_owned(),
                _ => "expr".to_owned(),
            };
            out.push(Att { attnum: 0, name, ty: Type::Text, typmod: -1, not_null: false, dropped: false, column: None });
        }
        out.extend(idx.include.iter().filter_map(|&c| t.columns.get(c)).map(|c| Att {
            attnum: 0,
            name: c.name.clone(),
            ty: c.ty.clone(),
            typmod: c.typmod,
            not_null: false,
            dropped: false,
            column: None,
        }));
        // Postgres disambiguates repeated names (`expr`, `expr1`, ...).
        let mut seen: Vec<String> = Vec::new();
        for (i, a) in out.iter_mut().enumerate() {
            a.attnum = i + 1;
            let base = a.name.clone();
            let mut n = 0;
            while seen.contains(&a.name) {
                n += 1;
                a.name = format!("{base}{n}");
            }
            seen.push(a.name.clone());
        }
        out
    }

    pub fn sequence_atts(seq: &SequenceDef) -> Vec<Att<'a>> {
        let _ = seq;
        [("last_value", Type::Int8), ("log_cnt", Type::Int8), ("is_called", Type::Bool)]
            .into_iter()
            .enumerate()
            .map(|(i, (n, t))| Att { attnum: i + 1, name: n.to_owned(), ty: t, typmod: -1, not_null: true, dropped: false, column: None })
            .collect()
    }

    /// Every constraint of every table, NOT NULL constraints included.
    pub fn constraints(&self) -> Vec<Con<'a>> {
        let mut out = Vec::new();
        for t in self.tables() {
            for c in &t.constraints {
                out.push(self.table_constraint(t, c));
            }
            for (i, col) in t.live_columns() {
                if col.not_null {
                    out.push(Con {
                        oid: not_null_oid(t.oid, i + 1),
                        name: col.not_null_name.clone().unwrap_or_else(|| not_null_constraint_name(&t.name, &col.name)),
                        contype: 'n',
                        table: t,
                        conkey: vec![i + 1],
                        confrelid: 0,
                        confkey: vec![],
                        conindid: 0,
                        on_update: ' ',
                        on_delete: ' ',
                        kind: None,
                    });
                }
            }
        }
        out
    }

    fn table_constraint(&self, t: &'a TableDef, c: &'a pgmem_catalog::Constraint) -> Con<'a> {
        let attnums = |cols: &[usize]| cols.iter().map(|c| c + 1).collect::<Vec<_>>();
        let index_named = |name: &str| t.indexes.iter().find(|i| i.name == name).map_or(0, |i| i.oid);
        let mut con = Con {
            oid: c.oid,
            name: c.name.clone(),
            contype: c.contype(),
            table: t,
            conkey: vec![],
            confrelid: 0,
            confkey: vec![],
            conindid: 0,
            on_update: ' ',
            on_delete: ' ',
            kind: Some(&c.kind),
        };
        match &c.kind {
            ConstraintKind::PrimaryKey { columns } | ConstraintKind::Unique { columns, .. } => {
                con.conkey = attnums(columns);
                con.conindid = index_named(&c.name);
            }
            ConstraintKind::Check { expr } => {
                con.conkey = check_columns(t, expr);
            }
            ConstraintKind::Exclude { elements, .. } => {
                con.conkey = elements.iter().flat_map(|(e, _)| check_columns(t, e)).collect();
                con.conindid = index_named(&c.name);
            }
            ConstraintKind::ForeignKey { columns, ref_table, ref_columns, on_delete, on_update, .. } => {
                con.conkey = attnums(columns);
                con.confkey = attnums(ref_columns);
                if let Some(rt) = self.catalog.table_q(ref_table) {
                    con.confrelid = rt.oid;
                    con.conindid = rt
                        .indexes
                        .iter()
                        .find(|i| (i.unique || i.primary) && i.exprs.is_empty() && i.columns == *ref_columns)
                        .map_or(0, |i| i.oid);
                }
                con.on_update = on_update.code();
                con.on_delete = on_delete.code();
            }
        }
        con
    }

    /// The constraint with this OID.
    pub fn constraint_by_oid(&self, oid: Oid) -> Option<Con<'a>> {
        self.constraints().into_iter().find(|c| c.oid == oid)
    }

    pub fn table_name_by_oid(&self, oid: Oid) -> Option<&'a str> {
        self.catalog.tables().find(|t| t.oid == oid).map(|t| t.name.as_str())
    }
}

/// Attribute numbers a CHECK expression references, in table order.
fn check_columns(t: &TableDef, e: &pgmem_ir::Expr) -> Vec<usize> {
    let mut names = Vec::new();
    collect_columns(e, &mut names);
    t.live_columns().filter(|(_, c)| names.contains(&c.name)).map(|(i, _)| i + 1).collect()
}

fn collect_columns(e: &pgmem_ir::Expr, out: &mut Vec<String>) {
    e.walk(&mut |x| {
        if let pgmem_ir::Expr::Column(parts) = x
            && let Some(last) = parts.last()
        {
            out.push(last.clone());
        }
    });
}

fn column_list(t: &TableDef, attnums: &[usize]) -> String {
    attnums.iter().filter_map(|a| t.columns.get(a - 1)).map(|c| quote_ident(&c.name)).collect::<Vec<_>>().join(", ")
}

/// `pg_get_constraintdef`.
pub fn constraint_def(model: &Model, con: &Con) -> String {
    let (deferrable, deferred, validated) = con.flags();
    let mut def = constraint_def_body(model, con);
    if deferrable {
        def.push_str(" DEFERRABLE");
    }
    if deferred {
        def.push_str(" INITIALLY DEFERRED");
    }
    if !validated {
        def.push_str(" NOT VALID");
    }
    def
}

fn constraint_def_body(model: &Model, con: &Con) -> String {
    match (con.contype, con.kind) {
        ('n', _) => format!("NOT NULL {}", column_list(con.table, &con.conkey)),
        ('p', Some(ConstraintKind::PrimaryKey { .. }) | None) => format!("PRIMARY KEY ({})", column_list(con.table, &con.conkey)),
        ('u', Some(ConstraintKind::Unique { nulls_not_distinct, .. })) => {
            let nnd = if *nulls_not_distinct { "NULLS NOT DISTINCT " } else { "" };
            format!("UNIQUE {nnd}({})", column_list(con.table, &con.conkey))
        }
        _ => {
            let constraint = con.table.constraints.iter().find(|c| c.oid == con.oid);
            constraint.and_then(|c| crate::deparse::deparse_constraint(model.catalog, con.table, c).ok()).unwrap_or_default()
        }
    }
}

/// `pg_get_indexdef`.
pub fn index_def(model: &Model, t: &TableDef, idx: &IndexDef) -> String {
    if let Ok(s) = crate::deparse::deparse_index(model.catalog, t, idx) {
        return s;
    }
    let mut keys: Vec<String> = idx.columns.iter().filter_map(|c| t.columns.get(*c)).map(|c| quote_ident(&c.name)).collect();
    for e in &idx.exprs {
        let text = crate::deparse::deparse_expr(model.catalog, t, e).unwrap_or_default();
        // Function calls print bare; any other expression gets its own parentheses.
        keys.push(if matches!(e, pgmem_ir::Expr::Function(_)) { text } else { format!("({text})") });
    }
    let mut s = format!(
        "CREATE {}INDEX {} ON public.{} USING {} ({})",
        if idx.unique { "UNIQUE " } else { "" },
        quote_ident(&idx.name),
        quote_ident(&t.name),
        idx.method,
        keys.join(", ")
    );
    if !idx.include.is_empty() {
        let cols: Vec<String> = idx.include.iter().filter_map(|c| t.columns.get(*c)).map(|c| quote_ident(&c.name)).collect();
        s.push_str(&format!(" INCLUDE ({})", cols.join(", ")));
    }
    if idx.nulls_not_distinct {
        s.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(p) = &idx.predicate {
        s.push_str(&format!(" WHERE {}", crate::deparse::deparse_expr(model.catalog, t, p).unwrap_or_default()));
    }
    s
}

/// `pg_attribute.attidentity`.
pub fn identity_char(c: Option<&ColumnDef>) -> char {
    match c.and_then(|c| c.identity) {
        Some(Identity::Always) => 'a',
        Some(Identity::ByDefault) => 'd',
        None => '\0',
    }
}

pub fn fk_action_rule(a: FkAction) -> &'static str {
    match a {
        FkAction::NoAction => "NO ACTION",
        FkAction::Restrict => "RESTRICT",
        FkAction::Cascade => "CASCADE",
        FkAction::SetNull => "SET NULL",
        FkAction::SetDefault => "SET DEFAULT",
    }
}
