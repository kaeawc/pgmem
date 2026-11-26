//! Row-level security DDL: CREATE / ALTER / DROP POLICY.

use crate::expr::lower_expr;
use crate::{ident, object_name};
use pgmem_ir::{AlterPolicy, CreatePolicy, PolicyRole, Statement};
use pgmem_types::SqlResult;
use sqlparser::ast as sq;

fn role(o: &sq::Owner) -> PolicyRole {
    match o {
        sq::Owner::Ident(i) if i.quote_style.is_none() && i.value.eq_ignore_ascii_case("public") => PolicyRole::Public,
        sq::Owner::Ident(i) => PolicyRole::Name(ident(i)),
        sq::Owner::CurrentRole | sq::Owner::CurrentUser => PolicyRole::CurrentUser,
        sq::Owner::SessionUser => PolicyRole::SessionUser,
    }
}

fn roles(to: Option<&Vec<sq::Owner>>) -> Option<Vec<PolicyRole>> {
    to.map(|v| v.iter().map(role).collect())
}

fn expr(e: Option<sq::Expr>) -> SqlResult<Option<pgmem_ir::Expr>> {
    e.map(lower_expr).transpose()
}

pub(crate) fn lower_create(p: sq::CreatePolicy) -> SqlResult<Statement> {
    let command = match p.command {
        None | Some(sq::CreatePolicyCommand::All) => '*',
        Some(sq::CreatePolicyCommand::Select) => 'r',
        Some(sq::CreatePolicyCommand::Insert) => 'a',
        Some(sq::CreatePolicyCommand::Update) => 'w',
        Some(sq::CreatePolicyCommand::Delete) => 'd',
    };
    Ok(Statement::CreatePolicy(Box::new(CreatePolicy {
        name: ident(&p.name),
        table: object_name(&p.table_name)?,
        permissive: !matches!(p.policy_type, Some(sq::CreatePolicyType::Restrictive)),
        command,
        roles: roles(p.to.as_ref()).unwrap_or_default(),
        using: expr(p.using)?,
        with_check: expr(p.with_check)?,
    })))
}

pub(crate) fn lower_alter(p: sq::AlterPolicy) -> SqlResult<Statement> {
    let mut out = AlterPolicy {
        name: ident(&p.name),
        table: object_name(&p.table_name)?,
        rename_to: None,
        roles: None,
        using: None,
        with_check: None,
    };
    match p.operation {
        sq::AlterPolicyOperation::Rename { new_name } => out.rename_to = Some(ident(&new_name)),
        sq::AlterPolicyOperation::Apply { to, using, with_check } => {
            out.roles = roles(to.as_ref());
            out.using = expr(using)?;
            out.with_check = expr(with_check)?;
        }
    }
    Ok(Statement::AlterPolicy(Box::new(out)))
}

pub(crate) fn lower_drop(p: &sq::DropPolicy) -> SqlResult<Statement> {
    Ok(Statement::DropPolicy { name: ident(&p.name), table: object_name(&p.table_name)?, if_exists: p.if_exists })
}
