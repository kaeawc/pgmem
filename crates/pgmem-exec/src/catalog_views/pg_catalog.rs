//! `pg_catalog` relations.

use super::model::{
    self, Att, C_COLLATION_OID, CLASS_CLASS_OID, CONSTRAINT_CLASS_OID, DEFAULT_COLLATION_OID, INFORMATION_SCHEMA_NAMESPACE_OID, Model,
    PG_DATABASE_OWNER_OID, SUPERUSER_OID, attrdef_oid, rowtype_oid,
};
use super::{Def, Spec, boolean, ch, int2, int4, name, oid, rows_of, snapshot, txt};
use crate::eval::EvalCtx;
use crate::plan::Row;
use pgmem_catalog::{Oid, PG_CATALOG_NAMESPACE_OID, PUBLIC_NAMESPACE_OID};
use pgmem_types::{RegKind, SqlResult, Type, Value};

const PG_TYPE_DATA: &str = include_str!("pg_type.tsv");
const PG_ROLES_DATA: &str = include_str!("pg_roles.tsv");
const PG_EXTENSIONS_DATA: &str = include_str!("pg_available_extensions.tsv");

const PG_NAMESPACE: &str = "oid:oid, nspname:name, nspowner:oid, nspacl:aclitem[]";
const PG_CLASS: &str = "oid:oid, relname:name, relnamespace:oid, reltype:oid=0, reloftype:oid=0, relowner:oid=10, relam:oid=0, \
    relfilenode:oid=0, reltablespace:oid=0, relpages:int4=0, reltuples:float4=-1, relallvisible:int4=0, relallfrozen:int4=0, \
    reltoastrelid:oid=0, relhasindex:bool=f, relisshared:bool=f, relpersistence:\"char\"=p, relkind:\"char\", relnatts:int2=0, \
    relchecks:int2=0, relhasrules:bool=f, relhastriggers:bool=f, relhassubclass:bool=f, relrowsecurity:bool=f, \
    relforcerowsecurity:bool=f, relispopulated:bool=t, relreplident:\"char\"=n, relispartition:bool=f, relrewrite:oid=0, \
    relfrozenxid:xid=0, relminmxid:xid=0, relacl:aclitem[], reloptions:text[], relpartbound:pg_node_tree";
const PG_ATTRIBUTE: &str = "attrelid:oid, attname:name, atttypid:oid, attlen:int2, attnum:int2, atttypmod:int4=-1, attndims:int2=0, \
    attbyval:bool, attalign:\"char\", attstorage:\"char\", attcompression:\"char\"=~, attnotnull:bool=f, atthasdef:bool=f, \
    atthasmissing:bool=f, attidentity:\"char\"=~, attgenerated:\"char\"=~, attisdropped:bool=f, attislocal:bool=t, \
    attinhcount:int2=0, attcollation:oid=0, attstattarget:int2, attacl:aclitem[], attoptions:text[], attfdwoptions:text[], \
    attmissingval:anyarray";
const PG_TYPE: &str = "oid:oid, typname:name, typnamespace:oid=11, typowner:oid=10, typlen:int2, typbyval:bool, typtype:\"char\"=b, \
    typcategory:\"char\", typispreferred:bool=f, typisdefined:bool=t, typdelim:\"char\", typrelid:oid=0, typsubscript:regproc, \
    typelem:oid=0, typarray:oid=0, typinput:regproc, typoutput:regproc, typreceive:regproc, typsend:regproc, typmodin:regproc, \
    typmodout:regproc, typanalyze:regproc, typalign:\"char\", typstorage:\"char\", typnotnull:bool=f, typbasetype:oid=0, \
    typtypmod:int4=-1, typndims:int4=0, typcollation:oid=0, typdefaultbin:pg_node_tree, typdefault:text, typacl:aclitem[]";
const PG_INDEX: &str = "indexrelid:oid, indrelid:oid, indnatts:int2, indnkeyatts:int2, indisunique:bool, indnullsnotdistinct:bool=f, \
    indisprimary:bool, indisexclusion:bool=f, indimmediate:bool=t, indisclustered:bool=f, indisvalid:bool=t, indcheckxmin:bool=f, \
    indisready:bool=t, indislive:bool=t, indisreplident:bool=f, indkey:int2vector, indcollation:oidvector, indclass:oidvector, \
    indoption:int2vector, indexprs:pg_node_tree, indpred:pg_node_tree";
const PG_CONSTRAINT: &str = "oid:oid, conname:name, connamespace:oid=2200, contype:\"char\", condeferrable:bool=f, condeferred:bool=f, \
    conenforced:bool=t, convalidated:bool=t, conrelid:oid, contypid:oid=0, conindid:oid=0, conparentid:oid=0, confrelid:oid=0, \
    confupdtype:\"char\"= , confdeltype:\"char\"= , confmatchtype:\"char\"= , conislocal:bool=t, coninhcount:int2=0, \
    connoinherit:bool=f, conperiod:bool=f, conkey:int2[], confkey:int2[], conpfeqop:oid[], conppeqop:oid[], conffeqop:oid[], \
    confdelsetcols:int2[], conexclop:oid[], conbin:pg_node_tree";
const PG_ATTRDEF: &str = "oid:oid, adrelid:oid, adnum:int2, adbin:pg_node_tree";
const PG_DESCRIPTION: &str = "objoid:oid, classoid:oid, objsubid:int4, description:text";
const PG_AM: &str = "oid:oid, amname:name, amhandler:regproc, amtype:\"char\"";
const PG_COLLATION: &str = "oid:oid, collname:name, collnamespace:oid=11, collowner:oid=10, collprovider:\"char\", \
    collisdeterministic:bool=t, collencoding:int4, collcollate:text, collctype:text, colllocale:text, collicurules:text, collversion:text";
const PG_DATABASE: &str = "oid:oid, datname:name, datdba:oid=10, encoding:int4=6, datlocprovider:\"char\"=b, datistemplate:bool=f, \
    datallowconn:bool=t, dathasloginevt:bool=f, datconnlimit:int4=-1, datfrozenxid:xid=0, datminmxid:xid=0, dattablespace:oid=1663, \
    datcollate:text, datctype:text, datlocale:text, daticurules:text, datcollversion:text, datacl:aclitem[]";
const PG_ROLES: &str = "rolname:name, rolsuper:bool, rolinherit:bool, rolcreaterole:bool, rolcreatedb:bool, rolcanlogin:bool, \
    rolreplication:bool, rolconnlimit:int4=-1, rolpassword:text, rolvaliduntil:timestamptz, rolbypassrls:bool, rolconfig:text[], oid:oid";
const PG_AUTHID: &str = "oid:oid, rolname:name, rolsuper:bool, rolinherit:bool, rolcreaterole:bool, rolcreatedb:bool, \
    rolcanlogin:bool, rolreplication:bool, rolbypassrls:bool, rolconnlimit:int4=-1, rolpassword:text, rolvaliduntil:timestamptz";
const PG_USER: &str = "usename:name, usesysid:oid, usecreatedb:bool, usesuper:bool, userepl:bool, usebypassrls:bool, passwd:text, \
    valuntil:timestamptz, useconfig:text[]";
const PG_TABLESPACE: &str = "oid:oid, spcname:name, spcowner:oid=10, spcacl:aclitem[], spcoptions:text[]";
const PG_LANGUAGE: &str = "oid:oid, lanname:name, lanowner:oid=10, lanispl:bool, lanpltrusted:bool, lanplcallfoid:oid=0, \
    laninline:oid=0, lanvalidator:oid=0, lanacl:aclitem[]";
const PG_EXTENSION: &str = "oid:oid, extname:name, extowner:oid=10, extnamespace:oid=11, extrelocatable:bool=f, extversion:text, \
    extconfig:oid[], extcondition:text[]";
const PG_AVAILABLE_EXTENSIONS: &str = "name:name, default_version:text, installed_version:text, comment:text";
const PG_TABLES: &str = "schemaname:name, tablename:name, tableowner:name, tablespace:name, hasindexes:bool, hasrules:bool=f, \
    hastriggers:bool, rowsecurity:bool=f";
const PG_VIEWS: &str = "schemaname:name, viewname:name, viewowner:name, definition:text";
const PG_MATVIEWS: &str = "schemaname:name, matviewname:name, matviewowner:name, tablespace:name, hasindexes:bool, ispopulated:bool, \
    definition:text";
const PG_INDEXES: &str = "schemaname:name, tablename:name, indexname:name, tablespace:name, indexdef:text";
const PG_SEQUENCES: &str = "schemaname:name, sequencename:name, sequenceowner:name, data_type:regtype, start_value:int8, \
    min_value:int8, max_value:int8, increment_by:int8, cycle:bool=f, cache_size:int8=1, last_value:int8";
const PG_SEQUENCE: &str = "seqrelid:oid, seqtypid:oid, seqstart:int8, seqincrement:int8, seqmax:int8, seqmin:int8, seqcache:int8=1, \
    seqcycle:bool=f";
const PG_SETTINGS: &str = "name:text, setting:text, unit:text, category:text, short_desc:text, extra_desc:text, context:text, \
    vartype:text, source:text, min_val:text, max_val:text, enumvals:text[], boot_val:text, reset_val:text, sourcefile:text, \
    sourceline:int4, pending_restart:bool=f";
pub(super) const PG_PROC: &str = "oid:oid, proname:name, pronamespace:oid, proowner:oid, prolang:oid, procost:float4, prorows:float4, \
    provariadic:oid, prosupport:regproc, prokind:\"char\", prosecdef:bool, proleakproof:bool, proisstrict:bool, proretset:bool, \
    provolatile:\"char\", proparallel:\"char\", pronargs:int2, pronargdefaults:int2, prorettype:oid, proargtypes:oidvector, \
    proallargtypes:oid[], proargmodes:\"char\"[], proargnames:text[], proargdefaults:pg_node_tree, protrftypes:oid[], prosrc:text, \
    probin:text, prosqlbody:pg_node_tree, proconfig:text[], proacl:aclitem[]";
const PG_ENUM: &str = "oid:oid, enumtypid:oid, enumsortorder:float4, enumlabel:name";
pub(super) const PG_DEPEND: &str =
    "classid:oid, objid:oid, objsubid:int4, refclassid:oid, refobjid:oid, refobjsubid:int4, deptype:\"char\"";
const PG_POLICY: &str = "oid:oid, polname:name, polrelid:oid, polcmd:\"char\", polpermissive:bool, polroles:oid[], \
    polqual:pg_node_tree, polwithcheck:pg_node_tree";
const PG_PARTITIONED_TABLE: &str = "partrelid:oid, partstrat:\"char\", partnatts:int2, partdefid:oid, partattrs:int2vector, \
    partclass:oidvector, partcollation:oidvector, partexprs:pg_node_tree";
const PG_STATISTIC_EXT: &str = "oid:oid, stxrelid:oid, stxname:name, stxnamespace:oid, stxowner:oid, stxkeys:int2vector, \
    stxstattarget:int2, stxkind:\"char\"[], stxexprs:pg_node_tree";
const PG_PUBLICATION: &str = "oid:oid, pubname:name, pubowner:oid, puballtables:bool, pubinsert:bool, pubupdate:bool, pubdelete:bool, \
    pubtruncate:bool, pubviaroot:bool, pubgencols:\"char\"";
const PG_PUBLICATION_NAMESPACE: &str = "oid:oid, pnpubid:oid, pnnspid:oid";
const PG_PUBLICATION_REL: &str = "oid:oid, prpubid:oid, prrelid:oid, prqual:pg_node_tree, prattrs:int2vector";
const PG_INHERITS: &str = "inhrelid:oid, inhparent:oid, inhseqno:int4, inhdetachpending:bool";
pub(super) const PG_TRIGGER: &str = "oid:oid, tgrelid:oid, tgparentid:oid, tgname:name, tgfoid:oid, tgtype:int2, tgenabled:\"char\", \
    tgisinternal:bool, tgconstrrelid:oid, tgconstrindid:oid, tgconstraint:oid, tgdeferrable:bool, tginitdeferred:bool, tgnargs:int2, \
    tgattr:int2vector, tgargs:bytea, tgqual:pg_node_tree, tgoldtable:name, tgnewtable:name";
const PG_RANGE: &str = "rngtypid:oid, rngsubtype:oid, rngmultitypid:oid, rngcollation:oid, rngsubopc:oid, rngcanonical:regproc, \
    rngsubdiff:regproc";
const PG_PREPARED_STATEMENTS: &str = "name:text, statement:text, prepare_time:timestamptz, parameter_types:regtype[], \
    result_types:regtype[], from_sql:bool, generic_plans:int8, custom_plans:int8";
const PG_CURSORS: &str = "name:text, statement:text, is_holdable:bool, is_binary:bool, is_scrollable:bool, creation_time:timestamptz";
const PG_LOCKS: &str = "locktype:text, database:oid, relation:oid, page:int4, tuple:int2, virtualxid:text, transactionid:xid, \
    classid:oid, objid:oid, objsubid:int2, virtualtransaction:text, pid:int4, mode:text, granted:bool, fastpath:bool, \
    waitstart:timestamptz";
const PG_STAT_ACTIVITY: &str = "datid:oid, datname:name, pid:int4, leader_pid:int4, usesysid:oid, usename:name, application_name:text, \
    client_addr:inet, client_hostname:text, client_port:int4, backend_start:timestamptz, xact_start:timestamptz, \
    query_start:timestamptz, state_change:timestamptz, wait_event_type:text, wait_event:text, state:text, backend_xid:xid, \
    backend_xmin:xid, query_id:int8, query:text, backend_type:text";

macro_rules! def {
    ($name:literal, $oid:expr, $kind:literal, $spec:expr, $rows:expr) => {
        Def { schema: "pg_catalog", name: $name, oid: $oid, kind: $kind, spec: $spec, rows: $rows }
    };
}

/// The `pg_catalog` relations, with Postgres' OIDs for the catalog tables (views get stable
/// made-up ones; theirs vary between builds).
pub static DEFS: &[Def] = &[
    def!("pg_namespace", 2615, 'r', PG_NAMESPACE, pg_namespace),
    def!("pg_class", 1259, 'r', PG_CLASS, pg_class),
    def!("pg_attribute", 1249, 'r', PG_ATTRIBUTE, pg_attribute),
    def!("pg_type", 1247, 'r', PG_TYPE, pg_type),
    def!("pg_index", 2610, 'r', PG_INDEX, pg_index),
    def!("pg_constraint", 2606, 'r', PG_CONSTRAINT, pg_constraint),
    def!("pg_attrdef", 2604, 'r', PG_ATTRDEF, pg_attrdef),
    def!("pg_description", 2609, 'r', PG_DESCRIPTION, pg_description),
    def!("pg_am", 2601, 'r', PG_AM, pg_am),
    def!("pg_collation", 3456, 'r', PG_COLLATION, pg_collation),
    def!("pg_database", 1262, 'r', PG_DATABASE, pg_database),
    def!("pg_authid", 1260, 'r', PG_AUTHID, pg_authid),
    def!("pg_tablespace", 1213, 'r', PG_TABLESPACE, pg_tablespace),
    def!("pg_language", 2612, 'r', PG_LANGUAGE, pg_language),
    def!("pg_extension", 3079, 'r', PG_EXTENSION, pg_extension),
    def!("pg_proc", 1255, 'r', PG_PROC, super::routines::pg_proc),
    def!("pg_enum", 3501, 'r', PG_ENUM, pg_enum),
    def!("pg_depend", 2608, 'r', PG_DEPEND, super::depend::pg_depend),
    def!("pg_rewrite", 2618, 'r', super::depend::PG_REWRITE, super::depend::pg_rewrite),
    def!("pg_inherits", 2611, 'r', PG_INHERITS, pg_inherits),
    def!("pg_policy", 3256, 'r', PG_POLICY, empty),
    def!("pg_statistic_ext", 3381, 'r', PG_STATISTIC_EXT, empty),
    def!("pg_publication", 6104, 'r', PG_PUBLICATION, empty),
    def!("pg_publication_namespace", 6237, 'r', PG_PUBLICATION_NAMESPACE, empty),
    def!("pg_publication_rel", 6106, 'r', PG_PUBLICATION_REL, empty),
    def!("pg_partitioned_table", 3350, 'r', PG_PARTITIONED_TABLE, pg_partitioned_table),
    def!("pg_trigger", 2620, 'r', PG_TRIGGER, super::routines::pg_trigger),
    def!("pg_range", 3541, 'r', PG_RANGE, pg_range),
    def!("pg_sequence", 2224, 'r', PG_SEQUENCE, pg_sequence),
    def!("pg_roles", 12_001, 'v', PG_ROLES, pg_roles),
    def!("pg_user", 12_002, 'v', PG_USER, pg_user),
    def!("pg_tables", 12_003, 'v', PG_TABLES, pg_tables),
    def!("pg_views", 12_004, 'v', PG_VIEWS, pg_views),
    def!("pg_matviews", 12_005, 'v', PG_MATVIEWS, pg_matviews),
    def!("pg_indexes", 12_006, 'v', PG_INDEXES, pg_indexes),
    def!("pg_sequences", 12_007, 'v', PG_SEQUENCES, pg_sequences),
    def!("pg_settings", 12_008, 'v', PG_SETTINGS, pg_settings),
    def!("pg_available_extensions", 12_009, 'v', PG_AVAILABLE_EXTENSIONS, pg_available_extensions),
    def!("pg_prepared_statements", 12_010, 'v', PG_PREPARED_STATEMENTS, pg_prepared_statements),
    def!("pg_cursors", 12_011, 'v', PG_CURSORS, pg_cursors),
    def!("pg_locks", 12_012, 'v', PG_LOCKS, pg_locks),
    def!("pg_stat_activity", 12_013, 'v', PG_STAT_ACTIVITY, pg_stat_activity),
];

fn pg_inherits(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    Ok(rows_of(PG_INHERITS, |s| {
        m.tables()
            .into_iter()
            .filter_map(|t| m.catalog.partition_of(t.oid).map(|of| (t.oid, of.parent)))
            .map(|(child, parent)| {
                s.row(vec![
                    ("inhrelid", oid(child)),
                    ("inhparent", oid(parent)),
                    ("inhseqno", Value::Int4(1)),
                    ("inhdetachpending", boolean(false)),
                ])
            })
            .collect()
    }))
}

/// Default btree (403) and hash (405) operator classes by input type, recorded from Postgres 18.
const DEFAULT_OPCLASSES: &[(u32, u32, u32)] = &[
    (403, 16, 10003),
    (403, 17, 10006),
    (403, 18, 10007),
    (403, 19, 10028),
    (403, 20, 3124),
    (403, 21, 1979),
    (403, 23, 1978),
    (403, 25, 3126),
    (403, 26, 1981),
    (403, 700, 10012),
    (403, 701, 3123),
    (403, 1042, 10004),
    (403, 1082, 3122),
    (403, 1083, 10038),
    (403, 1114, 3128),
    (403, 1184, 3127),
    (403, 1186, 10022),
    (403, 1266, 10041),
    (403, 1700, 3125),
    (403, 2950, 10065),
    (403, 3614, 10071),
    (403, 3802, 10088),
    (405, 16, 10048),
    (405, 17, 10049),
    (405, 18, 10008),
    (405, 19, 10029),
    (405, 20, 10021),
    (405, 21, 10019),
    (405, 23, 10020),
    (405, 25, 10037),
    (405, 26, 10031),
    (405, 700, 10013),
    (405, 701, 10014),
    (405, 1042, 10005),
    (405, 1082, 10011),
    (405, 1083, 10039),
    (405, 1114, 10046),
    (405, 1184, 10040),
    (405, 1186, 10023),
    (405, 1266, 10042),
    (405, 1700, 10030),
    (405, 2950, 10066),
    (405, 3802, 10089),
];

fn default_opclass(method: u32, ty: &Type) -> u32 {
    // varchar and name-like strings use text's operator class.
    let oid = if *ty == Type::Varchar { 25 } else { ty.base().oid() };
    DEFAULT_OPCLASSES.iter().find(|(m, t, _)| *m == method && *t == oid).map_or(0, |(_, _, o)| *o)
}

fn pg_partitioned_table(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    Ok(rows_of(PG_PARTITIONED_TABLE, |s| {
        let mut rows = Vec::new();
        for t in m.tables() {
            let Some(key) = m.catalog.partition_key(t.oid) else { continue };
            let default = m.catalog.partitions(t.oid).into_iter().find(|(_, of)| of.bound == pgmem_catalog::PartitionBound::Default);
            let attrs: Vec<Value> = key
                .parts
                .iter()
                .map(|p| match p {
                    pgmem_catalog::PartitionKeyPart::Column(slot) => Value::Int2(i16::try_from(slot + 1).unwrap_or(0)),
                    pgmem_catalog::PartitionKeyPart::Expr(_) => Value::Int2(0),
                })
                .collect();
            let exprs: Vec<String> = key
                .parts
                .iter()
                .filter_map(|p| match p {
                    pgmem_catalog::PartitionKeyPart::Expr(e) => crate::deparse::deparse_expr(m.catalog, t, e).ok(),
                    pgmem_catalog::PartitionKeyPart::Column(_) => None,
                })
                .collect();
            let n = key.parts.len();
            let binder = crate::bind::Binder::new(m.catalog, &[]);
            let method = if key.strategy == pgmem_catalog::PartitionStrategy::Hash { 405 } else { 403 };
            let classes: Vec<Value> = crate::partition::key_types(&binder, t, key)
                .unwrap_or_default()
                .iter()
                .map(|ty| Value::Oid(default_opclass(method, ty)))
                .collect();
            let collations: Vec<Value> = crate::partition::key_types(&binder, t, key)
                .unwrap_or_default()
                .iter()
                .map(|ty| Value::Oid(if ty.base().is_string() { model::DEFAULT_COLLATION_OID } else { 0 }))
                .collect();
            rows.push(s.row(vec![
                ("partrelid", oid(t.oid)),
                ("partstrat", ch(key.strategy.code())),
                ("partnatts", int2(n)),
                ("partdefid", oid(default.map_or(0, |(d, _)| d.oid))),
                ("partattrs", Value::Array(attrs)),
                ("partclass", Value::Array(classes)),
                ("partcollation", Value::Array(collations)),
                ("partexprs", if exprs.is_empty() { Value::Null } else { txt(exprs.join(", ")) }),
            ]));
        }
        rows
    }))
}

fn empty(_: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(Vec::new())
}

/// Builtin type facts recorded from Postgres 18 (`pg_type.tsv`).
pub struct BuiltinType<'a> {
    pub fields: Vec<&'a str>,
}

impl BuiltinType<'_> {
    pub fn oid(&self) -> u32 {
        self.fields[0].parse().unwrap_or(0)
    }
    fn field(&self, i: usize) -> &str {
        self.fields.get(i).copied().unwrap_or("")
    }
    pub fn len(&self) -> i16 {
        self.field(2).parse().unwrap_or(-1)
    }
    pub fn byval(&self) -> bool {
        self.field(3) == "t"
    }
    pub fn align(&self) -> char {
        self.field(15).chars().next().unwrap_or('i')
    }
    pub fn storage(&self) -> char {
        self.field(16).chars().next().unwrap_or('x')
    }
    pub fn collation(&self) -> u32 {
        self.field(17).parse().unwrap_or(0)
    }
}

pub fn builtin_types() -> impl Iterator<Item = BuiltinType<'static>> {
    PG_TYPE_DATA.lines().filter(|l| !l.is_empty()).map(|l| BuiltinType { fields: l.split('\t').collect() })
}

pub fn builtin_type(type_oid: u32) -> Option<BuiltinType<'static>> {
    builtin_types().find(|t| t.oid() == type_oid)
}

/// Storage facts of a type: (typlen, typbyval, typalign, typstorage, typcollation).
pub fn type_storage(t: &Type) -> (i16, bool, char, char, u32) {
    match t {
        Type::Enum(_) => return (4, true, 'i', 'p', 0),
        Type::Domain(_) => return type_storage(&t.base()),
        Type::Composite(_) | Type::Record => return (-1, false, 'd', 'x', 0),
        Type::Array(e) if matches!(**e, Type::Enum(_) | Type::Domain(_) | Type::Composite(_) | Type::Record) => {
            let align = if type_storage(e).2 == 'd' { 'd' } else { 'i' };
            return (-1, false, align, 'x', 0);
        }
        _ => {}
    }
    match builtin_type(t.oid()) {
        Some(b) => (b.len(), b.byval(), b.align(), b.storage(), b.collation()),
        None => (-1, false, 'i', 'x', 0),
    }
}

fn regproc(name: &str) -> Value {
    let oid = super::builtin_procs().iter().find(|p| p.name == name).map_or(0, |p| p.oid);
    Value::Reg(RegKind::Proc, oid, name.to_owned())
}

fn pg_namespace(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let mut all: Vec<(String, u32, u32)> = vec![
        ("pg_toast".into(), 99, SUPERUSER_OID),
        ("pg_catalog".into(), PG_CATALOG_NAMESPACE_OID, SUPERUSER_OID),
        ("information_schema".into(), INFORMATION_SCHEMA_NAMESPACE_OID, SUPERUSER_OID),
    ];
    for sc in m.catalog.schemas() {
        let owner = if sc.oid == PUBLIC_NAMESPACE_OID { PG_DATABASE_OWNER_OID } else { SUPERUSER_OID };
        all.push((sc.name.clone(), sc.oid, owner));
    }
    all.sort_by_key(|(_, o, _)| *o);
    Ok(rows_of(PG_NAMESPACE, |s| {
        all.iter().map(|(n, o, owner)| s.row(vec![("oid", oid(*o)), ("nspname", name(n)), ("nspowner", oid(*owner))])).collect()
    }))
}

/// A relation's `relacl` (NULL while it has its default privileges).
fn acl(ctx: &EvalCtx, m: &Model, rel: u32, sequence: bool) -> Value {
    let object = pgmem_catalog::GrantObject::Relation(rel);
    crate::roles::acl_items(m.catalog, ctx.env, &object, rel, sequence)
        .map_or(Value::Null, |items| Value::Array(items.into_iter().map(Value::Text).collect()))
}

fn class_row(s: &Spec, o: u32, relname: &str, ns: u32, kind: char, natts: usize, extra: Vec<(&'static str, Value)>) -> Row {
    let mut vals =
        vec![("oid", oid(o)), ("relname", name(relname)), ("relnamespace", oid(ns)), ("relkind", ch(kind)), ("relnatts", int2(natts))];
    vals.extend(extra);
    s.row(vals)
}

fn pg_class(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let triggers = fk_tables(&m);
    let s = Spec::parse(PG_CLASS);
    let mut rows = Vec::new();
    for t in m.tables() {
        let checks = t.constraints.iter().filter(|c| c.contype() == 'c').count();
        let partitioned = m.catalog.is_partitioned(t.oid);
        let bound = crate::partition::partition_bound_text(m.catalog, t);
        rows.push(class_row(
            &s,
            t.oid,
            &t.name,
            m.namespace(&t.schema),
            if t.matview.is_some() {
                'm'
            } else if partitioned {
                'p'
            } else {
                'r'
            },
            t.columns.len(),
            vec![
                ("reltype", oid(rowtype_oid(t.oid))),
                ("relam", oid(if partitioned { 0 } else { 2 })),
                ("relfilenode", oid(if partitioned { 0 } else { t.oid })),
                ("relacl", acl(ctx, &m, t.oid, false)),
                ("reloptions", crate::reloptions::as_value(&t.reloptions)),
                ("relispartition", boolean(bound.is_some())),
                ("relpartbound", bound.map_or(Value::Null, txt)),
                ("relhasindex", boolean(!t.indexes.is_empty())),
                ("relhasrules", boolean(t.matview.is_some())),
                ("relchecks", int2(checks)),
                ("relhastriggers", boolean(triggers.contains(&t.oid) || !m.catalog.table_triggers(t.oid).is_empty())),
                ("relreplident", ch(t.replident)),
                (
                    "relpersistence",
                    ch(if t.schema.starts_with("pg_temp_") {
                        't'
                    } else if t.unlogged {
                        'u'
                    } else {
                        'p'
                    }),
                ),
            ],
        ));
    }
    for v in m.views() {
        rows.push(class_row(
            &s,
            v.oid,
            &v.name,
            m.namespace(&v.schema),
            'v',
            v.columns.len(),
            vec![
                ("reltype", oid(rowtype_oid(v.oid))),
                ("relacl", acl(ctx, &m, v.oid, false)),
                ("relhasrules", boolean(true)),
                ("reloptions", crate::reloptions::as_value(&v.reloptions)),
            ],
        ));
    }
    for seq in m.sequences() {
        rows.push(class_row(
            &s,
            seq.oid,
            &seq.name,
            m.namespace(&seq.schema),
            'S',
            3,
            vec![("relfilenode", oid(seq.oid)), ("relacl", acl(ctx, &m, seq.oid, true))],
        ));
    }
    for t in m.catalog.types() {
        if let pgmem_catalog::TypeDefKind::Composite { fields, relid } = &t.kind {
            rows.push(class_row(&s, *relid, &t.name, m.namespace(&t.schema), 'c', fields.len(), vec![("reltype", oid(t.oid))]));
        }
    }
    for (t, idx) in m.indexes() {
        let natts = idx.columns.len() + idx.exprs.len() + idx.include.len();
        let partitioned = m.catalog.is_partitioned(t.oid);
        rows.push(class_row(
            &s,
            idx.oid,
            &idx.name,
            m.namespace(&t.schema),
            if partitioned { 'I' } else { 'i' },
            natts,
            vec![
                ("relam", oid(403)),
                ("relfilenode", oid(if partitioned { 0 } else { idx.oid })),
                ("reloptions", crate::reloptions::as_value(&idx.reloptions)),
            ],
        ));
    }
    for d in super::defs() {
        let ns = if d.schema == "pg_catalog" { PG_CATALOG_NAMESPACE_OID } else { INFORMATION_SCHEMA_NAMESPACE_OID };
        let natts = Spec::parse(d.spec).columns().len();
        let extra = if d.kind == 'r' { vec![("relam", oid(2)), ("relfilenode", oid(d.oid))] } else { vec![] };
        rows.push(class_row(&s, d.oid, d.name, ns, d.kind, natts, extra));
    }
    Ok(rows)
}

/// Tables with foreign keys in either direction (Postgres implements them with triggers).
fn fk_tables(m: &Model) -> Vec<u32> {
    let mut out = Vec::new();
    for c in m.constraints().iter().filter(|c| c.contype == 'f') {
        out.push(c.table.oid);
        out.push(c.confrelid);
    }
    out
}

fn att_row(s: &Spec, relid: u32, a: &Att) -> Row {
    let (len, byval, align, storage, type_collation) = type_storage(&a.ty);
    let column = a.column;
    let collation = column
        .and_then(|c| c.collation.as_deref())
        .and_then(|n| crate::bind::ddl::COLLATIONS.iter().find(|(c, _)| *c == n))
        .map_or(type_collation, |(_, o)| *o);
    let mut vals = vec![
        ("attrelid", oid(relid)),
        ("attname", name(&a.name)),
        ("atttypid", oid(if a.dropped { 0 } else { a.ty.oid() })),
        ("attlen", Value::Int2(len)),
        ("attnum", int2(a.attnum)),
        ("atttypmod", Value::Int4(a.typmod)),
        ("attndims", int2(usize::from(matches!(a.ty, Type::Array(_))))),
        ("attbyval", boolean(byval)),
        ("attalign", ch(align)),
        ("attstorage", ch(column.and_then(|c| c.storage).unwrap_or(storage))),
        ("attcompression", ch(column.and_then(|c| c.compression).unwrap_or('\0'))),
        ("attnotnull", boolean(a.not_null)),
        ("attisdropped", boolean(a.dropped)),
        ("attcollation", oid(collation)),
    ];
    if let Some(c) = column {
        vals.push(("attstattarget", c.stattarget.map_or(Value::Null, Value::Int2)));
        vals.push(("atthasdef", boolean(!c.dropped && (c.default.is_some() || c.generated.is_some()))));
        vals.push(("atthasmissing", boolean(!c.missing.is_null())));
        vals.push(("attidentity", ch(model::identity_char(Some(c)))));
        vals.push(("attgenerated", ch(if c.generated.is_some() { 's' } else { '\0' })));
    }
    s.row(vals)
}

/// The system columns every table has (`ctid`, `xmin`, ...), with Postgres' attributes.
fn system_att_rows(s: &Spec, relid: u32) -> Vec<Row> {
    [("tableoid", -6, 26, 4, true, 'i'), ("cmax", -5, 29, 4, true, 'i'), ("xmax", -4, 28, 4, true, 'i')]
        .into_iter()
        .chain([("cmin", -3, 29, 4, true, 'i'), ("xmin", -2, 28, 4, true, 'i'), ("ctid", -1, 27, 6, false, 's')])
        .map(|(n, num, typ, len, byval, align)| {
            s.row(vec![
                ("attrelid", oid(relid)),
                ("attname", name(n)),
                ("atttypid", oid(typ)),
                ("attlen", Value::Int2(len)),
                ("attnum", Value::Int2(num)),
                ("attbyval", boolean(byval)),
                ("attalign", ch(align)),
                ("attstorage", ch('p')),
                ("attnotnull", boolean(true)),
            ])
        })
        .collect()
}

fn pg_attribute(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_ATTRIBUTE);
    let mut rows = Vec::new();
    // Column privileges (`attacl`) of table and view columns.
    let attacl = s.columns().iter().position(|(n, _)| n == "attacl");
    let with_acl = |relid: u32, a: &Att| {
        let mut row = att_row(&s, relid, a);
        let items = i16::try_from(a.attnum).ok().filter(|_| !a.dropped).and_then(|n| crate::roles::column_acl_items(m.catalog, relid, n));
        if let (Some(i), Some(items)) = (attacl, items) {
            row[i] = Value::Array(items.into_iter().map(Value::Text).collect());
        }
        row
    };
    for t in m.tables() {
        rows.extend(system_att_rows(&s, t.oid));
        rows.extend(Model::table_atts(t).iter().map(|a| with_acl(t.oid, a)));
    }
    for v in m.views() {
        rows.extend(m.view_atts(v).iter().map(|a| with_acl(v.oid, a)));
    }
    for seq in m.sequences() {
        rows.extend(Model::sequence_atts(seq).iter().map(|a| att_row(&s, seq.oid, a)));
    }
    for t in m.catalog.types() {
        if let pgmem_catalog::TypeDefKind::Composite { fields, relid } = &t.kind {
            for (i, f) in fields.iter().enumerate() {
                let a = Att {
                    attnum: i + 1,
                    name: f.name.clone(),
                    ty: f.ty.clone(),
                    typmod: f.typmod,
                    not_null: false,
                    dropped: false,
                    column: None,
                };
                rows.push(att_row(&s, *relid, &a));
            }
        }
    }
    for (t, idx) in m.indexes() {
        rows.extend(Model::index_atts(t, idx).iter().map(|a| att_row(&s, idx.oid, a)));
    }
    Ok(rows)
}

fn pg_type(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_TYPE);
    let mut rows: Vec<Row> = builtin_types()
        .map(|b| {
            let f = |i: usize| b.field(i).to_owned();
            let o = |i: usize| oid(b.field(i).parse().unwrap_or(0));
            s.row(vec![
                ("oid", oid(b.oid())),
                ("typname", name(b.field(1))),
                ("typlen", Value::Int2(b.len())),
                ("typbyval", boolean(b.byval())),
                ("typtype", ch(b.field(4).chars().next().unwrap_or('b'))),
                ("typcategory", ch(b.field(5).chars().next().unwrap_or('U'))),
                ("typispreferred", boolean(b.field(6) == "t")),
                ("typdelim", ch(',')),
                ("typelem", o(7)),
                ("typarray", o(8)),
                ("typinput", regproc(&f(9))),
                ("typoutput", regproc(&f(10))),
                ("typreceive", regproc(&f(11))),
                ("typsend", regproc(&f(12))),
                ("typmodin", regproc(&f(13))),
                ("typmodout", regproc(&f(14))),
                ("typalign", ch(b.align())),
                ("typstorage", ch(b.storage())),
                ("typcollation", oid(b.collation())),
                ("typsubscript", regproc(&f(18))),
                ("typanalyze", regproc(&f(19))),
            ])
        })
        .collect();
    // Every table and view has a composite row type of the same name.
    let rels: Vec<(u32, &str, &str)> = m
        .tables()
        .into_iter()
        .map(|t| (t.oid, t.name.as_str(), t.schema.as_str()))
        .chain(m.views().into_iter().map(|v| (v.oid, v.name.as_str(), v.schema.as_str())))
        .collect();
    for (relid, relname, schema) in rels {
        rows.push(s.row(vec![
            ("oid", oid(rowtype_oid(relid))),
            ("typname", name(relname)),
            ("typnamespace", oid(m.namespace(schema))),
            ("typlen", Value::Int2(-1)),
            ("typbyval", boolean(false)),
            ("typtype", ch('c')),
            ("typcategory", ch('C')),
            ("typdelim", ch(',')),
            ("typrelid", oid(relid)),
            ("typarray", oid(super::depend::rowtype_array_oid(relid))),
            ("typinput", regproc("record_in")),
            ("typoutput", regproc("record_out")),
            ("typreceive", regproc("record_recv")),
            ("typsend", regproc("record_send")),
            ("typmodin", regproc("-")),
            ("typmodout", regproc("-")),
            ("typsubscript", regproc("-")),
            ("typanalyze", regproc("-")),
            ("typalign", ch('d')),
            ("typstorage", ch('x')),
        ]));
        // ... and its array type.
        rows.push(s.row(vec![
            ("oid", oid(super::depend::rowtype_array_oid(relid))),
            ("typname", name(&format!("_{relname}"))),
            ("typnamespace", oid(m.namespace(schema))),
            ("typlen", Value::Int2(-1)),
            ("typbyval", boolean(false)),
            ("typcategory", ch('A')),
            ("typdelim", ch(',')),
            ("typelem", oid(rowtype_oid(relid))),
            ("typinput", regproc("array_in")),
            ("typoutput", regproc("array_out")),
            ("typreceive", regproc("array_recv")),
            ("typsend", regproc("array_send")),
            ("typmodin", regproc("-")),
            ("typmodout", regproc("-")),
            ("typsubscript", regproc("array_subscript_handler")),
            ("typanalyze", regproc("array_typanalyze")),
            ("typalign", ch('d')),
            ("typstorage", ch('x')),
        ]));
    }
    for t in m.catalog.types() {
        rows.extend(user_type_rows(&s, m.catalog, t));
    }
    rows.extend(range_type_rows(&s));
    rows.extend(extension_type_rows(&s, &m));
    Ok(rows)
}

/// A varlena base or array type row (ranges, multiranges, extension types).
#[allow(clippy::too_many_arguments)]
fn varlena_type_row(
    s: &Spec,
    o: u32,
    typname: &str,
    namespace: u32,
    typtype: char,
    category: char,
    elem: u32,
    array: u32,
    io: &str,
) -> Row {
    s.row(vec![
        ("oid", oid(o)),
        ("typname", name(typname)),
        ("typnamespace", oid(namespace)),
        ("typlen", Value::Int2(-1)),
        ("typbyval", boolean(false)),
        ("typtype", ch(typtype)),
        ("typcategory", ch(category)),
        ("typdelim", ch(',')),
        ("typelem", oid(elem)),
        ("typarray", oid(array)),
        ("typinput", regproc(&format!("{io}_in"))),
        ("typoutput", regproc(&format!("{io}_out"))),
        ("typreceive", regproc(&format!("{io}_recv"))),
        ("typsend", regproc(&format!("{io}_send"))),
        ("typmodin", regproc("-")),
        ("typmodout", regproc("-")),
        ("typsubscript", regproc(if category == 'A' { "array_subscript_handler" } else { "-" })),
        ("typanalyze", regproc("-")),
        ("typalign", ch('i')),
        ("typstorage", ch('x')),
    ])
}

/// The builtin range and multirange types and their arrays.
fn range_type_rows(s: &Spec) -> Vec<Row> {
    let mut rows = Vec::new();
    for (_, r, m, ra, ma, rname, mname) in pgmem_types::range::RANGES {
        rows.push(varlena_type_row(s, *r, rname, PG_CATALOG_NAMESPACE_OID, 'r', 'R', 0, *ra, "range"));
        rows.push(varlena_type_row(s, *m, mname, PG_CATALOG_NAMESPACE_OID, 'm', 'R', 0, *ma, "multirange"));
        rows.push(varlena_type_row(s, *ra, &format!("_{rname}"), PG_CATALOG_NAMESPACE_OID, 'b', 'A', *r, 0, "array"));
        rows.push(varlena_type_row(s, *ma, &format!("_{mname}"), PG_CATALOG_NAMESPACE_OID, 'b', 'A', *m, 0, "array"));
    }
    rows
}

/// citext / hstore, once their extension is installed (in its schema).
fn extension_type_rows(s: &Spec, m: &Model) -> Vec<Row> {
    let mut rows = Vec::new();
    for (ext, o, typname, category) in
        [("citext", pgmem_types::CITEXT_OID, "citext", 'S'), ("hstore", pgmem_types::HSTORE_OID, "hstore", 'U')]
    {
        let Some(e) = m.catalog.extension(ext) else { continue };
        let ns = m.namespace(&e.schema);
        rows.push(varlena_type_row(s, o, typname, ns, 'b', category, 0, o + 1, typname));
        rows.push(varlena_type_row(s, o + 1, &format!("_{typname}"), ns, 'b', 'A', o, 0, "array"));
    }
    rows
}

fn pg_range(_: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(rows_of(PG_RANGE, |s| {
        pgmem_types::range::RANGES
            .iter()
            .map(|(sub, r, m, ..)| {
                let discrete = matches!(sub, Type::Int4 | Type::Int8 | Type::Date);
                let canonical =
                    if discrete { format!("{}_canonical", Type::Range(Box::new(sub.clone())).typname()) } else { "-".to_owned() };
                s.row(vec![
                    ("rngtypid", oid(*r)),
                    ("rngsubtype", oid(sub.oid())),
                    ("rngmultitypid", oid(*m)),
                    ("rngcollation", oid(0)),
                    ("rngsubopc", oid(0)),
                    ("rngcanonical", regproc(&canonical)),
                    ("rngsubdiff", regproc("-")),
                ])
            })
            .collect()
    }))
}

/// The `pg_type` rows of a user type and of its array type, as `TypeCreate` fills them.
fn user_type_rows(s: &Spec, catalog: &pgmem_catalog::Catalog, t: &pgmem_catalog::TypeDef) -> Vec<Row> {
    use pgmem_catalog::TypeDefKind;
    let common = |vals: &mut Vec<(&'static str, Value)>| {
        vals.extend([
            ("typnamespace", oid(catalog.namespace_oid(&t.schema).unwrap_or(PUBLIC_NAMESPACE_OID))),
            ("typdelim", ch(',')),
            ("typmodin", regproc("-")),
            ("typmodout", regproc("-")),
        ]);
    };
    let mut vals: Vec<(&'static str, Value)> =
        vec![("oid", oid(t.oid)), ("typname", name(&t.name)), ("typtype", ch(t.typtype())), ("typarray", oid(t.array_oid))];
    common(&mut vals);
    let collation = match &t.kind {
        TypeDefKind::Domain { base, .. } => type_storage(base).4,
        _ => 0,
    };
    let align = match &t.kind {
        TypeDefKind::Enum { .. } => {
            vals.extend([
                ("typlen", Value::Int2(4)),
                ("typbyval", boolean(true)),
                ("typcategory", ch('E')),
                ("typinput", regproc("enum_in")),
                ("typoutput", regproc("enum_out")),
                ("typreceive", regproc("enum_recv")),
                ("typsend", regproc("enum_send")),
                ("typsubscript", regproc("-")),
                ("typanalyze", regproc("-")),
                ("typalign", ch('i')),
                ("typstorage", ch('p')),
            ]);
            'i'
        }
        TypeDefKind::Composite { relid, .. } => {
            vals.extend([
                ("typlen", Value::Int2(-1)),
                ("typbyval", boolean(false)),
                ("typcategory", ch('C')),
                ("typrelid", oid(*relid)),
                ("typinput", regproc("record_in")),
                ("typoutput", regproc("record_out")),
                ("typreceive", regproc("record_recv")),
                ("typsend", regproc("record_send")),
                ("typsubscript", regproc("-")),
                ("typanalyze", regproc("-")),
                ("typalign", ch('d')),
                ("typstorage", ch('x')),
            ]);
            'd'
        }
        TypeDefKind::Domain { base, typmod, not_null, default, .. } => {
            let (len, byval, align, storage, collation) = type_storage(base);
            // I/O functions come from the ultimate (non-domain) base type.
            let b = builtin_type(base.base().oid());
            let fname = |i: usize, fallback: &str| b.as_ref().map_or_else(|| fallback.to_owned(), |b| b.field(i).to_owned());
            let default_text = default.as_ref().and_then(|d| {
                let empty = pgmem_catalog::TableDef {
                    oid: 0,
                    schema: String::new(),
                    name: String::new(),
                    columns: vec![],
                    constraints: vec![],
                    indexes: vec![],
                    matview: None,
                    unlogged: false,
                    replident: 'd',
                    reloptions: Vec::new(),
                };
                let mut col = pgmem_catalog::ColumnDef::new("value", base.clone());
                col.typmod = *typmod;
                col.default = Some(d.clone());
                crate::deparse::deparse_default(catalog, &empty, &col).ok().flatten()
            });
            vals.extend([
                ("typlen", Value::Int2(len)),
                ("typbyval", boolean(byval)),
                ("typcategory", ch(base.category())),
                ("typinput", regproc("domain_in")),
                ("typoutput", regproc(&fname(10, "record_out"))),
                ("typreceive", regproc("domain_recv")),
                ("typsend", regproc(&fname(12, "record_send"))),
                ("typsubscript", regproc(&fname(18, "-"))),
                ("typanalyze", regproc(&fname(19, "-"))),
                ("typalign", ch(align)),
                ("typstorage", ch(storage)),
                ("typnotnull", boolean(*not_null)),
                ("typbasetype", oid(base.oid())),
                ("typtypmod", Value::Int4(*typmod)),
                ("typcollation", oid(collation)),
                ("typdefault", default_text.map_or(Value::Null, Value::Text)),
            ]);
            align
        }
    };
    let mut arr: Vec<(&'static str, Value)> = vec![
        ("oid", oid(t.array_oid)),
        ("typname", name(&format!("_{}", t.name))),
        ("typlen", Value::Int2(-1)),
        ("typbyval", boolean(false)),
        ("typtype", ch('b')),
        ("typcategory", ch('A')),
        ("typelem", oid(t.oid)),
        ("typinput", regproc("array_in")),
        ("typoutput", regproc("array_out")),
        ("typreceive", regproc("array_recv")),
        ("typsend", regproc("array_send")),
        ("typsubscript", regproc("array_subscript_handler")),
        ("typanalyze", regproc("array_typanalyze")),
        ("typalign", ch(if align == 'd' { 'd' } else { 'i' })),
        ("typstorage", ch('x')),
        ("typcollation", oid(collation)),
    ];
    common(&mut arr);
    vec![s.row(vals), s.row(arr)]
}

fn pg_enum(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_ENUM);
    let mut rows = Vec::new();
    for t in m.catalog.types() {
        if let pgmem_catalog::TypeDefKind::Enum { labels } = &t.kind {
            for l in labels {
                rows.push(s.row(vec![
                    ("oid", oid(l.oid)),
                    ("enumtypid", oid(t.oid)),
                    ("enumsortorder", Value::Float4(l.sort)),
                    ("enumlabel", name(&l.label)),
                ]));
            }
        }
    }
    Ok(rows)
}

fn pg_index(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_INDEX);
    Ok(m.indexes()
        .into_iter()
        .map(|(t, idx)| {
            let attnums = idx.key_attnums(t);
            let mut keys: Vec<Value> = attnums.iter().map(|a| int2(*a)).collect();
            let collations: Vec<Value> = attnums
                .iter()
                .map(|a| {
                    oid(a
                        .checked_sub(1)
                        .and_then(|c| t.columns.get(c))
                        .map_or(0, |col| if col.ty.is_string() { DEFAULT_COLLATION_OID } else { 0 }))
                })
                .collect();
            let nkey = keys.len();
            keys.extend(idx.include.iter().map(|c| int2(c + 1)));
            let n = keys.len();
            let exprs = (!idx.exprs.is_empty()).then(|| {
                idx.true_exprs(t).map(|e| crate::deparse::deparse_expr(m.catalog, t, e).unwrap_or_default()).collect::<Vec<_>>().join(", ")
            });
            let pred = idx.predicate.as_ref().map(|p| crate::deparse::deparse_predicate(m.catalog, t, p).unwrap_or_default());
            s.row(vec![
                ("indexrelid", oid(idx.oid)),
                ("indrelid", oid(t.oid)),
                ("indnatts", int2(n)),
                ("indnkeyatts", int2(nkey)),
                ("indisunique", boolean(idx.unique)),
                ("indnullsnotdistinct", boolean(idx.nulls_not_distinct)),
                ("indisprimary", boolean(idx.primary)),
                ("indisclustered", boolean(idx.clustered)),
                ("indisreplident", boolean(idx.replica)),
                ("indkey", Value::Array(keys)),
                ("indcollation", Value::Array(collations)),
                ("indclass", Value::Array(vec![oid(0); nkey])),
                ("indoption", Value::Array(vec![Value::Int2(0); nkey])),
                ("indexprs", exprs.map_or(Value::Null, txt)),
                ("indpred", pred.map_or(Value::Null, txt)),
            ])
        })
        .collect())
}

fn int2_array(attnums: &[usize]) -> Value {
    Value::Array(attnums.iter().map(|a| int2(*a)).collect())
}

/// A constraint of a domain, as `pg_constraint` records it.
pub struct DomainCon {
    pub oid: Oid,
    pub name: String,
    pub contype: char,
    pub validated: bool,
    /// The CHECK expression as ruleutils prints it (`(VALUE > 0)`); None for NOT NULL.
    pub check: Option<String>,
}

impl DomainCon {
    /// `pg_get_constraintdef`.
    pub fn def(&self) -> String {
        match &self.check {
            Some(e) => format!("CHECK ({e})"),
            None => "NOT NULL".to_owned(),
        }
    }
}

/// The constraints of a domain: its NOT NULL (Postgres 17+ records it as
/// `<domain>_not_null`), then its CHECKs.
pub fn domain_constraints(catalog: &pgmem_catalog::Catalog, t: &pgmem_catalog::TypeDef) -> Vec<DomainCon> {
    let pgmem_catalog::TypeDefKind::Domain { base, typmod, not_null, checks, .. } = &t.kind else { return Vec::new() };
    let mut out = Vec::new();
    if *not_null {
        out.push(DomainCon {
            oid: domain_not_null_oid(t.oid),
            name: format!("{}_not_null", t.name),
            contype: 'n',
            validated: true,
            check: None,
        });
    }
    for c in checks {
        let check = crate::deparse::deparse_domain_check(catalog, base, *typmod, &c.expr).unwrap_or_default();
        out.push(DomainCon { oid: c.oid, name: c.name.clone(), contype: 'c', validated: c.validated, check: Some(check) });
    }
    out
}

/// The OID of a domain's NOT NULL constraint (pgmem stores the flag, not a constraint).
pub fn domain_not_null_oid(domain: Oid) -> Oid {
    0x7000_0000_u32.wrapping_add(domain)
}

fn pg_constraint(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_CONSTRAINT);
    let mut domain_rows = Vec::new();
    for t in m.catalog.types() {
        for c in domain_constraints(m.catalog, t) {
            let mut vals = vec![
                ("oid", oid(c.oid)),
                ("conname", name(&c.name)),
                ("connamespace", oid(m.namespace(&t.schema))),
                ("contype", ch(c.contype)),
                ("conrelid", oid(0)),
                ("contypid", oid(t.oid)),
                ("convalidated", boolean(c.validated)),
            ];
            if let Some(e) = c.check {
                vals.push(("conbin", txt(e)));
            }
            domain_rows.push(s.row(vals));
        }
    }
    let mut rows: Vec<Row> = m
        .constraints()
        .iter()
        .map(|c| {
            let mut vals = vec![
                ("oid", oid(c.oid)),
                ("conname", name(&c.name)),
                ("connamespace", oid(m.namespace(&c.table.schema))),
                ("contype", ch(c.contype)),
                ("conrelid", oid(c.table.oid)),
                ("conindid", oid(c.conindid)),
                ("conkey", int2_array(&c.conkey)),
            ];
            let (deferrable, deferred, validated) = c.flags();
            vals.extend([("condeferrable", boolean(deferrable)), ("condeferred", boolean(deferred)), ("convalidated", boolean(validated))]);
            if c.contype == 'f' {
                vals.extend([
                    ("confrelid", oid(c.confrelid)),
                    ("confupdtype", ch(c.on_update)),
                    ("confdeltype", ch(c.on_delete)),
                    (
                        "confmatchtype",
                        ch(if matches!(c.kind, Some(pgmem_catalog::ConstraintKind::ForeignKey { match_full: true, .. })) {
                            'f'
                        } else {
                            's'
                        }),
                    ),
                    ("confkey", int2_array(&c.confkey)),
                ]);
            }
            if let Some(pgmem_catalog::ConstraintKind::Check { expr }) = c.kind {
                vals.push(("conbin", txt(crate::deparse::deparse_predicate(m.catalog, c.table, expr).unwrap_or_default())));
            }
            s.row(vals)
        })
        .collect();
    rows.extend(domain_rows);
    Ok(rows)
}

fn pg_attrdef(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_ATTRDEF);
    let mut rows = Vec::new();
    for t in m.tables() {
        for (i, c) in t.live_columns() {
            let text = if c.generated.is_some() {
                crate::deparse::deparse_generated(m.catalog, t, c)
            } else {
                crate::deparse::deparse_default(m.catalog, t, c)
            };
            let Some(text) = text.ok().flatten() else { continue };
            rows.push(s.row(vec![
                ("oid", oid(attrdef_oid(t.oid, i + 1))),
                ("adrelid", oid(t.oid)),
                ("adnum", int2(i + 1)),
                // pgmem keeps the deparsed text here; pg_get_expr returns it.
                ("adbin", txt(text)),
            ]));
        }
    }
    Ok(rows)
}

fn object_exists(m: &Model, classoid: u32, objoid: u32) -> bool {
    match classoid {
        CLASS_CLASS_OID => m.catalog.relation_by_oid(objoid).is_some(),
        CONSTRAINT_CLASS_OID => m.constraint_by_oid(objoid).is_some(),
        _ => true,
    }
}

fn pg_description(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_DESCRIPTION);
    Ok(m.catalog
        .comments()
        .filter(|((class, obj, _), _)| object_exists(&m, *class, *obj))
        .map(|((class, obj, sub), text)| {
            s.row(vec![
                ("objoid", oid(*obj)),
                ("classoid", oid(*class)),
                ("objsubid", Value::Int4(*sub)),
                ("description", txt(text.clone())),
            ])
        })
        .collect())
}

fn pg_am(_: &EvalCtx) -> SqlResult<Vec<Row>> {
    let rows = [(2, "heap", "heap_tableam_handler", 't'), (403, "btree", "bthandler", 'i'), (405, "hash", "hashhandler", 'i')]
        .into_iter()
        .chain([(783, "gist", "gisthandler", 'i'), (2742, "gin", "ginhandler", 'i'), (3580, "brin", "brinhandler", 'i')])
        .chain([(4000, "spgist", "spghandler", 'i')]);
    Ok(rows_of(PG_AM, |s| {
        rows.map(|(o, n, h, t)| s.row(vec![("oid", oid(o)), ("amname", name(n)), ("amhandler", regproc(h)), ("amtype", ch(t))])).collect()
    }))
}

fn pg_collation(_: &EvalCtx) -> SqlResult<Vec<Row>> {
    let rows = [
        (DEFAULT_COLLATION_OID, "default", 'd', -1, None, None, None),
        (811, "pg_c_utf8", 'b', 6, None, None, Some("C.UTF-8")),
        (C_COLLATION_OID, "C", 'c', -1, Some("C"), Some("C"), None),
        (951, "POSIX", 'c', -1, Some("POSIX"), Some("POSIX"), None),
        (962, "ucs_basic", 'b', 6, None, None, Some("C")),
    ];
    Ok(rows_of(PG_COLLATION, |s| {
        rows.into_iter()
            .map(|(o, n, p, enc, collate, ctype, locale)| {
                let opt = |v: Option<&str>| v.map_or(Value::Null, txt);
                s.row(vec![
                    ("oid", oid(o)),
                    ("collname", name(n)),
                    ("collprovider", ch(p)),
                    ("collencoding", Value::Int4(enc)),
                    ("collcollate", opt(collate)),
                    ("collctype", opt(ctype)),
                    ("colllocale", opt(locale)),
                ])
            })
            .collect()
    }))
}

fn pg_database(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let current = ctx.env.database.clone();
    let mut dbs = vec![(5, current.clone(), false)];
    if current != "template1" {
        dbs.push((1, "template1".to_owned(), true));
    }
    dbs.push((4, "template0".to_owned(), true));
    Ok(rows_of(PG_DATABASE, |s| {
        dbs.into_iter()
            .map(|(o, n, template)| {
                s.row(vec![
                    ("oid", oid(o)),
                    ("datname", name(&n)),
                    ("datistemplate", boolean(template)),
                    ("datallowconn", boolean(o != 4)),
                    ("datcollate", txt("C.UTF-8")),
                    ("datctype", txt("C.UTF-8")),
                    ("datlocale", txt("C.UTF-8")),
                ])
            })
            .collect()
    }))
}

/// A predefined role (`pg_read_all_data`, ...): its OID.
pub fn predefined_role(name: &str) -> Option<u32> {
    PG_ROLES_DATA.lines().filter(|l| !l.is_empty()).find_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        (f.get(1) == Some(&name)).then(|| f[0].parse().unwrap_or(0))
    })
}

/// (oid, name, super, inherit, createrole, createdb, canlogin, replication, bypassrls,
/// connection limit): the session user, the predefined roles, then created roles.
fn roles(ctx: &EvalCtx) -> Vec<(u32, String, [bool; 7], i32)> {
    let mut out = vec![(SUPERUSER_OID, ctx.env.user.clone(), [true; 7], -1)];
    for l in PG_ROLES_DATA.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = l.split('\t').collect();
        let b = |i: usize| f.get(i).is_some_and(|v| *v == "t");
        out.push((f[0].parse().unwrap_or(0), f[1].to_owned(), [b(2), b(3), b(4), b(5), b(6), b(7), b(8)], -1));
    }
    for r in ctx.txn.catalog().roles() {
        out.push((
            r.oid,
            r.name.clone(),
            [r.superuser, r.inherit, r.createrole, r.createdb, r.login, r.replication, r.bypassrls],
            r.connlimit,
        ));
    }
    out
}

fn pg_roles(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let all = roles(ctx);
    Ok(rows_of(PG_ROLES, |s| {
        all.iter()
            .map(|(o, n, f, limit)| {
                s.row(vec![
                    ("rolname", name(n)),
                    ("rolconnlimit", int4(i64::from(*limit))),
                    ("rolsuper", boolean(f[0])),
                    ("rolinherit", boolean(f[1])),
                    ("rolcreaterole", boolean(f[2])),
                    ("rolcreatedb", boolean(f[3])),
                    ("rolcanlogin", boolean(f[4])),
                    ("rolreplication", boolean(f[5])),
                    ("rolbypassrls", boolean(f[6])),
                    ("rolpassword", txt("********")),
                    ("oid", oid(*o)),
                ])
            })
            .collect()
    }))
}

fn pg_authid(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let all = roles(ctx);
    Ok(rows_of(PG_AUTHID, |s| {
        all.iter()
            .map(|(o, n, f, limit)| {
                s.row(vec![
                    ("oid", oid(*o)),
                    ("rolconnlimit", int4(i64::from(*limit))),
                    ("rolname", name(n)),
                    ("rolsuper", boolean(f[0])),
                    ("rolinherit", boolean(f[1])),
                    ("rolcreaterole", boolean(f[2])),
                    ("rolcreatedb", boolean(f[3])),
                    ("rolcanlogin", boolean(f[4])),
                    ("rolreplication", boolean(f[5])),
                    ("rolbypassrls", boolean(f[6])),
                ])
            })
            .collect()
    }))
}

fn pg_user(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(rows_of(PG_USER, |s| {
        vec![s.row(vec![
            ("usename", name(&ctx.env.user)),
            ("usesysid", oid(SUPERUSER_OID)),
            ("usecreatedb", boolean(true)),
            ("usesuper", boolean(true)),
            ("userepl", boolean(true)),
            ("usebypassrls", boolean(true)),
            ("passwd", txt("********")),
        ])]
    }))
}

fn pg_tablespace(_: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(rows_of(PG_TABLESPACE, |s| {
        vec![
            s.row(vec![("oid", oid(1663)), ("spcname", name("pg_default"))]),
            s.row(vec![("oid", oid(1664)), ("spcname", name("pg_global"))]),
        ]
    }))
}

fn pg_language(_: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(rows_of(PG_LANGUAGE, |s| {
        [(12, "internal", false, false), (13, "c", false, false), (14, "sql", false, true), (13_562, "plpgsql", true, true)]
            .into_iter()
            .map(|(o, n, pl, trusted)| {
                s.row(vec![("oid", oid(o)), ("lanname", name(n)), ("lanispl", boolean(pl)), ("lanpltrusted", boolean(trusted))])
            })
            .collect()
    }))
}

/// An extension Postgres 18 ships: (name, default version).
pub fn available_extension(ext: &str) -> Option<(&'static str, &'static str)> {
    PG_EXTENSIONS_DATA.lines().filter(|l| !l.is_empty()).find_map(|l| {
        let mut f = l.splitn(3, '\t');
        let n = f.next()?;
        let v = f.next()?;
        (n == ext).then_some((n, v))
    })
}

fn pg_extension(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    Ok(rows_of(PG_EXTENSION, |s| {
        let mut rows = vec![s.row(vec![("oid", oid(13_558)), ("extname", name("plpgsql")), ("extversion", txt("1.0"))])];
        for e in m.catalog.extensions() {
            rows.push(s.row(vec![
                ("oid", oid(e.oid)),
                ("extname", name(&e.name)),
                ("extnamespace", oid(m.namespace(&e.schema))),
                ("extversion", txt(e.version.clone())),
                ("extrelocatable", boolean(true)),
            ]));
        }
        rows
    }))
}

fn pg_available_extensions(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let catalog = ctx.txn.catalog();
    Ok(rows_of(PG_AVAILABLE_EXTENSIONS, |s| {
        PG_EXTENSIONS_DATA
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let f: Vec<&str> = l.splitn(3, '\t').collect();
                let get = |i: usize| f.get(i).copied().unwrap_or("");
                let installed = match catalog.extension(get(0)) {
                    Some(e) => txt(e.version.clone()),
                    None if get(0) == "plpgsql" => txt(get(1)),
                    None => Value::Null,
                };
                s.row(vec![
                    ("name", name(get(0))),
                    ("default_version", txt(get(1))),
                    ("installed_version", installed),
                    ("comment", txt(get(2))),
                ])
            })
            .collect()
    }))
}

fn pg_sequence(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_SEQUENCE);
    Ok(m.sequences()
        .into_iter()
        .map(|seq| {
            s.row(vec![
                ("seqrelid", oid(seq.oid)),
                ("seqtypid", oid(model::sequence_type(seq).oid())),
                ("seqstart", Value::Int8(seq.start)),
                ("seqincrement", Value::Int8(seq.increment)),
                ("seqmax", Value::Int8(seq.max)),
                ("seqmin", Value::Int8(seq.min)),
                ("seqcache", Value::Int8(seq.cache)),
                ("seqcycle", Value::Bool(seq.cycle)),
            ])
        })
        .collect())
}

fn pg_tables(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let triggers = fk_tables(&m);
    let s = Spec::parse(PG_TABLES);
    Ok(m.tables()
        .into_iter()
        .filter(|t| t.matview.is_none())
        .map(|t| {
            s.row(vec![
                ("schemaname", name(&t.schema)),
                ("tablename", name(&t.name)),
                ("tableowner", name(&ctx.env.user)),
                ("hasindexes", boolean(!t.indexes.is_empty())),
                ("hastriggers", boolean(triggers.contains(&t.oid))),
            ])
        })
        .collect())
}

fn pg_views(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_VIEWS);
    Ok(m.views()
        .into_iter()
        .map(|v| {
            s.row(vec![
                ("schemaname", name(&v.schema)),
                ("viewname", name(&v.name)),
                ("viewowner", name(&ctx.env.user)),
                ("definition", txt(crate::deparse::deparse_view(m.catalog, v).unwrap_or_default())),
            ])
        })
        .collect())
}

fn pg_matviews(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_MATVIEWS);
    Ok(m.tables()
        .into_iter()
        .filter_map(|t| t.matview.as_ref().map(|mv| (t, mv)))
        .map(|(t, mv)| {
            let as_view = pgmem_catalog::ViewDef {
                oid: t.oid,
                schema: t.schema.clone(),
                name: t.name.clone(),
                columns: t.live_columns().map(|(_, c)| c.name.clone()).collect(),
                query: mv.query.clone(),
                search_path: mv.search_path.clone(),
                check_option: false,
                reloptions: Vec::new(),
            };
            s.row(vec![
                ("schemaname", name(&t.schema)),
                ("matviewname", name(&t.name)),
                ("matviewowner", name(&ctx.env.user)),
                ("hasindexes", boolean(!t.indexes.is_empty())),
                ("ispopulated", boolean(mv.populated)),
                ("definition", txt(crate::deparse::deparse_view(m.catalog, &as_view).unwrap_or_default())),
            ])
        })
        .collect())
}

fn pg_indexes(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_INDEXES);
    Ok(m.indexes()
        .into_iter()
        .map(|(t, idx)| {
            s.row(vec![
                ("schemaname", name(&t.schema)),
                ("tablename", name(&t.name)),
                ("indexname", name(&idx.name)),
                ("indexdef", txt(model::index_def(&m, t, idx))),
            ])
        })
        .collect())
}

fn pg_sequences(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_SEQUENCES);
    Ok(m.sequences()
        .into_iter()
        .map(|seq| {
            let ty = model::sequence_type(seq);
            let last = ctx.txn.database().sequence_last_value(seq).map_or(Value::Null, Value::Int8);
            s.row(vec![
                ("schemaname", name(&seq.schema)),
                ("sequencename", name(&seq.name)),
                ("sequenceowner", name(&ctx.env.user)),
                ("data_type", Value::Reg(RegKind::Type, ty.oid(), ty.name())),
                ("start_value", Value::Int8(seq.start)),
                ("min_value", Value::Int8(seq.min)),
                ("max_value", Value::Int8(seq.max)),
                ("increment_by", Value::Int8(seq.increment)),
                ("cycle", Value::Bool(seq.cycle)),
                ("cache_size", Value::Int8(seq.cache)),
                ("last_value", last),
            ])
        })
        .collect())
}

fn pg_settings(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let s = Spec::parse(PG_SETTINGS);
    let defaults = crate::session::default_settings();
    let opt = |v: Option<&str>| v.map_or(Value::Null, txt);
    let mut rows = Vec::new();
    for d in crate::settings::defs() {
        let key = d.name.to_ascii_lowercase();
        let reset = defaults.get(&key).cloned().or_else(|| d.boot.map(str::to_owned));
        let value = ctx.state.setting(ctx.env, &key).or_else(|| reset.clone());
        let source = if ctx.env.settings.get(&key).is_some_and(|v| defaults.get(&key) != Some(v)) { "session" } else { "default" };
        let enumvals = if d.enumvals.is_empty() {
            Value::Null
        } else {
            Value::Array(d.enumvals.iter().map(|e| Value::Text((*e).to_owned())).collect())
        };
        rows.push(s.row(vec![
            ("name", txt(d.name)),
            ("setting", opt(value.as_deref())),
            ("unit", opt(d.unit)),
            ("category", txt(d.category)),
            ("short_desc", opt(d.short_desc)),
            ("extra_desc", opt(d.extra_desc)),
            ("context", txt(d.context)),
            ("vartype", txt(d.vartype)),
            ("source", txt(source)),
            ("min_val", opt(d.min)),
            ("max_val", opt(d.max)),
            ("enumvals", enumvals),
            ("boot_val", opt(d.boot)),
            ("reset_val", opt(reset.as_deref())),
        ]));
    }
    Ok(rows)
}

fn regtypes(types: &[Type]) -> Value {
    Value::Array(types.iter().map(|t| Value::Reg(RegKind::Type, t.oid(), t.name())).collect())
}

fn pg_prepared_statements(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(rows_of(PG_PREPARED_STATEMENTS, |s| {
        ctx.env
            .objects
            .prepared
            .iter()
            .map(|(n, p)| {
                let result_types =
                    p.columns.as_ref().map_or(Value::Null, |cols| regtypes(&cols.iter().map(|c| c.ty.clone()).collect::<Vec<_>>()));
                s.row(vec![
                    ("name", txt(n)),
                    ("statement", txt(&p.text)),
                    ("prepare_time", Value::Timestamptz(p.prepare_time)),
                    ("parameter_types", regtypes(&p.param_types)),
                    ("result_types", result_types),
                    ("from_sql", boolean(p.from_sql)),
                    ("generic_plans", Value::Int8(0)),
                    ("custom_plans", Value::Int8(0)),
                ])
            })
            .collect()
    }))
}

fn pg_cursors(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(rows_of(PG_CURSORS, |s| {
        ctx.env
            .objects
            .cursors
            .iter()
            .map(|c| {
                s.row(vec![
                    ("name", txt(&c.name)),
                    ("statement", txt(&c.text)),
                    ("is_holdable", boolean(c.hold)),
                    ("is_binary", boolean(c.binary)),
                    ("is_scrollable", boolean(c.scroll)),
                    ("creation_time", Value::Timestamptz(c.creation_time)),
                ])
            })
            .collect()
    }))
}

/// Advisory locks of every session (other lock types are not tracked).
fn pg_locks(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let held = ctx.txn.database().advisory().held();
    Ok(rows_of(PG_LOCKS, |s| {
        held.iter()
            .map(|l| {
                s.row(vec![
                    ("locktype", txt("advisory")),
                    ("database", oid(5)),
                    ("classid", oid(l.key.0)),
                    ("objid", oid(l.key.1)),
                    ("objsubid", int2(usize::from(l.key.2))),
                    ("pid", Value::Int4(l.pid)),
                    ("mode", txt(if l.shared { "ShareLock" } else { "ExclusiveLock" })),
                    ("granted", boolean(true)),
                    ("fastpath", boolean(false)),
                ])
            })
            .collect()
    }))
}

fn pg_stat_activity(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(rows_of(PG_STAT_ACTIVITY, |s| {
        vec![s.row(vec![
            ("datid", oid(5)),
            ("datname", name(&ctx.env.database)),
            ("pid", Value::Int4(ctx.env.backend_pid)),
            ("usesysid", oid(SUPERUSER_OID)),
            ("usename", name(&ctx.env.user)),
            ("application_name", txt(ctx.env.setting("application_name").unwrap_or_default())),
            ("backend_start", Value::Timestamptz(ctx.env.txn_start)),
            ("xact_start", Value::Timestamptz(ctx.env.txn_start)),
            ("query_start", Value::Timestamptz(ctx.env.stmt_start)),
            ("state_change", Value::Timestamptz(ctx.env.stmt_start)),
            ("state", txt("active")),
            ("backend_type", txt("client backend")),
        ])]
    }))
}
