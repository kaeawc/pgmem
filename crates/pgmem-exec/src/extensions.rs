//! CREATE / DROP EXTENSION. Every extension Postgres 18 ships can be installed (recorded in
//! `pg_extension`); pgmem implements the functions and types of pgcrypto, uuid-ossp, citext,
//! pg_trgm, hstore and btree_gist, which are available whether or not they are installed.

use crate::eval::StatementState;
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::ExtensionDef;
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, Type, code};

fn notice(tag: &str, message: String) -> QueryResult {
    QueryResult { tag: tag.to_owned(), notices: vec![Notice { severity: "NOTICE", code: "00000", message }], ..Default::default() }
}

/// The types an extension provides (its columns depend on it).
fn extension_types(name: &str) -> &'static [Type] {
    match name {
        "citext" => &[Type::CiText],
        "hstore" => &[Type::HStore],
        _ => &[],
    }
}

pub(crate) fn create_extension(
    name: &str,
    if_not_exists: bool,
    schema: Option<&str>,
    version: Option<&str>,
    txn: &mut Txn,
) -> SqlResult<QueryResult> {
    let Some((_, default_version)) = crate::catalog_views::available_extension(name) else {
        return Err(SqlError::feature_not_supported(format!("extension \"{name}\" is not available"))
            .with_detail(format!(
                "Could not open extension control file \"/usr/share/postgresql/18/extension/{name}.control\": No such file or directory."
            ))
            .with_hint("The extension must first be installed on the system where PostgreSQL is running."));
    };
    if name == "plpgsql" || txn.catalog().extension(name).is_some() {
        if if_not_exists {
            return Ok(notice("CREATE EXTENSION", format!("extension \"{name}\" already exists, skipping")));
        }
        return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("extension \"{name}\" already exists")));
    }
    let schema = match schema {
        Some(s) => {
            if txn.catalog().schema(s).is_none() {
                return Err(pgmem_catalog::undefined_schema(s));
            }
            s.to_owned()
        }
        None => txn.catalog().creation_schema(&ir::ObjectName(vec![name.to_owned()]))?,
    };
    let oid = txn.catalog_mut().alloc_oid();
    let version = version.map_or_else(|| default_version.to_owned(), str::to_owned);
    txn.catalog_mut().put_extension(ExtensionDef { oid, name: name.to_owned(), schema, version });
    Ok(QueryResult { tag: "CREATE EXTENSION".into(), ..Default::default() })
}

pub(crate) fn drop_extension(
    names: &[String],
    if_exists: bool,
    cascade: bool,
    txn: &mut Txn,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let mut notices = Vec::new();
    for name in names {
        if txn.catalog().extension(name).is_none() {
            if if_exists {
                notices.push(Notice {
                    severity: "NOTICE",
                    code: "00000",
                    message: format!("extension \"{name}\" does not exist, skipping"),
                });
                continue;
            }
            return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("extension \"{name}\" does not exist")));
        }
        // Columns of the extension's types depend on it.
        let types = extension_types(name);
        let mut users = Vec::new();
        for t in txn.catalog().tables() {
            for (slot, c) in t.live_columns() {
                if types.contains(&c.ty) || c.ty.element().is_some_and(|e| types.contains(e)) {
                    users.push((t.qualified(), slot, c.name.clone(), t.name.clone(), c.ty.name()));
                }
            }
        }
        if !users.is_empty() && !cascade {
            let lines: Vec<String> = users.iter().map(|(_, _, c, t, ty)| format!("column {c} of table {t} depends on type {ty}")).collect();
            return Err(SqlError::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop extension {name} because other objects depend on it"),
            )
            .with_detail(lines.join("\n"))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        for (table, slot, column, tname, _) in users {
            if let Some(def) = txn.catalog().table_q(&table).cloned() {
                let mut def = (*def).clone();
                let col = &mut def.columns[slot];
                col.dropped = true;
                col.name = format!("........pg.dropped.{}........", slot + 1);
                col.not_null = false;
                col.default = None;
                txn.catalog_mut().replace_table(&table, def);
                notices.push(Notice {
                    severity: "NOTICE",
                    code: "00000",
                    message: format!("drop cascades to column {column} of table {tname}"),
                });
            }
        }
        if let Some(oid) = txn.catalog().extension(name).map(|e| e.oid) {
            crate::ext_alter::drop_members(oid, cascade, txn, env, state)?;
        }
        // Functions marked DEPENDS ON EXTENSION go with it (no CASCADE needed); members were
        // dropped above, so each is dropped once.
        let marked: Vec<u32> = txn.catalog().functions().filter(|f| f.extensions.contains(name)).map(|f| f.oid).collect();
        for oid in marked {
            txn.catalog_mut().drop_comments(oid);
            txn.catalog_mut().drop_function(oid);
        }
        txn.catalog_mut().drop_extension(name);
    }
    Ok(QueryResult { tag: "DROP EXTENSION".into(), notices, ..Default::default() })
}
