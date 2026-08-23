// SPDX-License-Identifier: BSL-1.1

//! Las seis sentencias administrativas de cuentas locales: crear y borrar
//! usuario, cambiar contraseña, activar y desactivar, dar y quitar privilegio
//! de administrador, y listar.
//!
//! # Por qué son públicas
//!
//! Por decisión de producto: **la autenticación básica no se esconde tras el
//! muro de pago**. Un servidor de la edición pública que no pudiera gestionar
//! sus propios usuarios no serviría para nada.
//!
//! Es el mismo criterio que reparte las doce sentencias seis y seis: éstas
//! operan sobre cuentas, que toda edición tiene; las otras seis operan sobre el
//! catálogo de bases y los permisos por base, que sólo existen pagando.
//!
//! # Qué necesita, y qué no
//!
//! Sólo la **gestión de usuarios**, no la identidad completa. Es deliberado: la
//! edición pública no trae permisos ni catálogo, así que pedir aquí la
//! superficie ancha la haría depender de maquinaria que no existe en su
//! edición.

use ermya_graph::gql::AdminStatement;
use ermya_graph_protocol::packstream::PackStreamValue;

use crate::admin_dispatch::AdminPending;
use crate::audit::{AdminAction, AuditSink};
use crate::auth::{AuthStoreError, SecretString};

/// Dispatch a **user-management** statement with the narrow identity surface.
///
/// The Community path. Only the six account statements reach here — the caller
/// routes the other six elsewhere, or rejects them — so `&dyn UserStore` is
/// everything this needs. Grants and the catalogue never enter.
///
/// Exists because gating the whole dispatcher on the paid manager left a
/// Community server unable to create or list its own users, which the master
/// plan rejects outright. See [`needs_paid_edition`] for the split.
///
/// # Errors
///
/// `(bolt_code, message)`, same shape as [`dispatch_with_registry`].
///
/// # Panics
///
/// Panics if handed a statement that [`needs_paid_edition`] classifies as
/// paid. That is a caller bug, not a runtime condition: the caller must route
/// those away before reaching here.
pub async fn dispatch_user_admin(
    stmt: AdminStatement,
    store: &dyn crate::auth::UserStore,
    caller_user: &str,
    caller_is_admin: bool,
    conn_id: u64,
    audit: &AuditSink,
) -> Result<AdminPending, (String, String)> {
    // No hace falta comprobar que la sentencia sea de cuentas: quien llama es
    // el punto de extensión, y cada edición monta el suyo. Las que esta edición
    // no sirve ni siquiera llegan hasta aquí.
    dispatch_users_only(stmt, store, caller_user, caller_is_admin, conn_id, audit).await
}

/// Registra el desenlace de una mutación de cuenta y lo traduce al protocolo.
///
/// Las cinco sentencias que mutan una cuenta —crear, borrar, cambiar
/// contraseña, activar, marcar administrador— comparten forma: llamar al
/// almacén, y según el resultado emitir un evento de auditoría propio o
/// traducir el error. Sólo cambian el método y el evento, así que el patrón se
/// escribe una vez.
fn finish_user_mutation(
    outcome: Result<(), crate::auth::AuthStoreError>,
    action: AdminAction,
    op: &'static str,
    caller_user: &str,
    conn_id: u64,
    audit: &AuditSink,
) -> Result<AdminPending, (String, String)> {
    match outcome {
        Ok(()) => {
            audit.admin_action(conn_id, caller_user, action);
            Ok(empty())
        }
        Err(e) => Err(record_failure(audit, conn_id, caller_user, op, &e)),
    }
}

/// `SHOW USERS`: proyección de las cuentas locales. Gestión de usuarios, así
/// que Community. Extraída para que `dispatch_users_only` quepa en el tope de
/// líneas del linter — el cuerpo arma filas y es el más largo de los seis.
async fn dispatch_show_users(
    store: &dyn crate::auth::UserStore,
    caller_user: &str,
    conn_id: u64,
    audit: &AuditSink,
) -> Result<AdminPending, (String, String)> {
    let users = store.list_users().await.map_err(|e| {
        let pair = (
            "Neo.DatabaseError.General.UnknownError".to_owned(),
            e.to_string(),
        );
        audit.admin_action(
            conn_id,
            caller_user,
            AdminAction::Failed {
                reason: format!("list_users: {e}"),
            },
        );
        pair
    })?;

    audit.admin_action(conn_id, caller_user, AdminAction::ShowUsers);

    let rows: Vec<Vec<PackStreamValue>> = users
        .into_iter()
        .map(|u| {
            vec![
                PackStreamValue::String(u.username),
                PackStreamValue::Bool(u.enabled),
                PackStreamValue::Bool(u.is_admin),
                PackStreamValue::String(u.created_at),
            ]
        })
        .collect();

    Ok(AdminPending {
        fields_psv: vec![
            PackStreamValue::String("user".to_owned()),
            PackStreamValue::String("enabled".to_owned()),
            PackStreamValue::String("is_admin".to_owned()),
            PackStreamValue::String("created_at".to_owned()),
        ],
        rows,
    })
}

/// Las seis sentencias de cuentas locales, con la superficie de identidad
/// estrecha. Cuerpo compartido por las dos vías de despacho.
///
/// Toma `&dyn UserStore`: crear, borrar, cambiar contraseña, activar, marcar
/// administrador y listar no necesitan nada más. Ni concesiones ni catálogo
/// entran aquí, y por eso esta función sirve igual a un servidor Community —
/// que no tiene ninguna de las dos cosas — que a uno de pago.
///
/// # Errors
///
/// `(bolt_code, message)`.
pub(crate) async fn dispatch_users_only(
    stmt: AdminStatement,
    store: &dyn crate::auth::UserStore,
    caller_user: &str,
    caller_is_admin: bool,
    conn_id: u64,
    audit: &AuditSink,
) -> Result<AdminPending, (String, String)> {
    if !caller_is_admin && !is_non_admin_accessible(&stmt, caller_user) {
        audit.admin_action(conn_id, caller_user, AdminAction::ShowUsers);
        return Err((
            "Neo.ClientError.Security.Forbidden".to_owned(),
            "admin privileges required".to_owned(),
        ));
    }

    match stmt {
        AdminStatement::CreateUser { username, password } => {
            let secret = to_secret(password.into_bytes())?;
            let outcome = store.create_user(&username, &secret, false).await;
            finish_user_mutation(
                outcome,
                AdminAction::CreateUser { target: username },
                "create_user",
                caller_user,
                conn_id,
                audit,
            )
        }

        AdminStatement::DropUser { username } => {
            let outcome = store.drop_user(&username).await;
            finish_user_mutation(
                outcome,
                AdminAction::DropUser { target: username },
                "drop_user",
                caller_user,
                conn_id,
                audit,
            )
        }

        AdminStatement::AlterUserPassword { username, password } => {
            let secret = to_secret(password.into_bytes())?;
            let outcome = store.set_password(&username, &secret).await;
            finish_user_mutation(
                outcome,
                AdminAction::AlterUserPassword { target: username },
                "set_password",
                caller_user,
                conn_id,
                audit,
            )
        }

        AdminStatement::AlterUserStatus { username, enabled } => {
            let outcome = store.set_enabled(&username, enabled).await;
            finish_user_mutation(
                outcome,
                AdminAction::AlterUserStatus {
                    target: username,
                    enabled,
                },
                "set_enabled",
                caller_user,
                conn_id,
                audit,
            )
        }

        AdminStatement::AlterUserAdmin { username, is_admin } => {
            let outcome = store.set_admin(&username, is_admin).await;
            finish_user_mutation(
                outcome,
                AdminAction::AlterUserAdmin {
                    target: username,
                    is_admin,
                },
                "set_admin",
                caller_user,
                conn_id,
                audit,
            )
        }

        AdminStatement::ShowUsers => dispatch_show_users(store, caller_user, conn_id, audit).await,

        other => unreachable!("dispatch_users_only con sentencia de pago: {other:?}"),
    }
}

pub(crate) fn to_secret(bytes: Vec<u8>) -> Result<SecretString, (String, String)> {
    // The cypher admin parser only emits valid UTF-8, but we validate
    // defensively — dispatch is pub and could be driven directly by
    // non-parser callers in tests or future enterprise modules.
    let s = String::from_utf8(bytes).map_err(|e| {
        (
            "Neo.ClientError.Statement.ArgumentError".to_owned(),
            e.to_string(),
        )
    })?;
    Ok(SecretString::new(s))
}

pub(crate) fn record_failure(
    audit: &AuditSink,
    conn_id: u64,
    caller: &str,
    action: &'static str,
    e: &AuthStoreError,
) -> (String, String) {
    audit.admin_action(
        conn_id,
        caller,
        AdminAction::Failed {
            reason: format!("{action}: {e}"),
        },
    );
    map_auth_store_error(e)
}

/// Wire-code mapping shared by [`record_failure`] (legacy `admin_action`
/// path) and the Task 14 catalog event sites (`CreateDatabase`,
/// `DropDatabase`, `Grant`, `Revoke`). Those four sites emit their own
/// dedicated [`AuditEvent`] with `AuditOutcome::Failed { reason }`
/// before returning the Bolt error pair, so they must not double-emit
/// through `record_failure`.
///
/// The match below is exhaustive over `AuthStoreError` and uses no
/// catch-all arm. `AuthStoreError` is not `#[non_exhaustive]`, so
/// adding a new variant in the future will produce a compile error
/// here rather than silently routing through a default wire code.
/// That is deliberate: a new error class deserves a deliberate Bolt
/// code mapping, not the `UnknownError` fallback.
pub(crate) fn map_auth_store_error(e: &AuthStoreError) -> (String, String) {
    let code = match e {
        AuthStoreError::UserExists(_) | AuthStoreError::DatabaseExists(_) => {
            "Neo.ClientError.Schema.ConstraintValidationFailed"
        }
        AuthStoreError::UserNotFound(_) | AuthStoreError::DatabaseNotFound(_) => {
            "Neo.ClientError.Statement.EntityNotFound"
        }
        AuthStoreError::LastAdmin => "Neo.ClientError.Security.Forbidden",
        AuthStoreError::InvalidUsername { .. }
        | AuthStoreError::PasswordTooShort { .. }
        | AuthStoreError::PasswordTooLong { .. }
        | AuthStoreError::PasswordEmpty
        | AuthStoreError::InvalidDatabaseName { .. }
        | AuthStoreError::InvalidQuota { .. }
        | AuthStoreError::InvalidGrant { .. } => "Neo.ClientError.Statement.ArgumentError",
        AuthStoreError::Backend(_) => "Neo.DatabaseError.General.UnknownError",
    };
    (code.to_owned(), e.to_string())
}

pub(crate) fn empty() -> AdminPending {
    AdminPending {
        fields_psv: Vec::new(),
        rows: Vec::new(),
    }
}

pub(crate) fn is_non_admin_accessible(stmt: &AdminStatement, caller_user: &str) -> bool {
    matches!(
        stmt,
        AdminStatement::ShowGrants { filter_user: Some(u) } if u == caller_user
    )
}
