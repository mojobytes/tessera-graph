// SPDX-License-Identifier: BSL-1.1

//! El punto de extensión del despacho administrativo: **sólo la forma**.
//!
//! # Qué problema resuelve
//!
//! Las doce sentencias de administración se reparten seis y seis: las de
//! cuentas locales son públicas —la autenticación básica no se esconde tras el
//! muro de pago— y las de catálogo de bases y permisos son de la edición de
//! pago.
//!
//! Antes ese reparto lo decidía **el propio camino de consulta**: preguntaba a
//! un módulo de pago si la sentencia era de pago, miraba si había gestor
//! concreto, y elegía entre dos despachadores. Tres decisiones sobre ediciones
//! metidas en el camino que sirve las consultas normales.
//!
//! Eso tenía dos problemas. El técnico: el árbol público llamaba a un módulo
//! que no viaja, así que no compilaba una vez copiado. Y el de fondo, peor:
//! **añadir una sentencia de pago obligaba a tocar el árbol público**, porque
//! allí vivía la lista de cuáles lo eran.
//!
//! # Cómo se resuelve
//!
//! El mismo patrón que el resto del arranque: **el despachador se inyecta**.
//! Quien monta el servidor decide cuál pone, y el camino de consulta se limita
//! a entregarle la sentencia sin saber qué edición está sirviendo.
//!
//! - La edición pública monta [`CommunityAdminDispatcher`], que sirve las seis
//!   de cuentas y responde "esa función no existe aquí" a las demás.
//! - La de pago monta el suyo, que sirve las doce.
//!
//! El camino público deja de contener la lista de sentencias de pago, el
//! `if` sobre la presencia del gestor y la llamada al módulo que se va.
//!
//! # Por qué falla cerrado y no devuelve vacío
//!
//! Cuando llega una sentencia que esta edición no sirve, la respuesta es un
//! error explícito. Devolver una lista de permisos vacía se leería como "no hay
//! permisos concedidos" cuando la verdad es "esta edición no tiene permisos".
//! Un cliente no puede distinguir esas dos cosas, y la primera es una mentira
//! con consecuencias de seguridad.

use std::sync::Arc;

use ermya_graph::gql::AdminStatement;
use ermya_graph_protocol::packstream::PackStreamValue;

use crate::audit::AuditSink;
use crate::auth::UserStore;

/// Lo que un despachador administrativo devuelve cuando la sentencia se sirve:
/// las columnas y sus filas, ya en la forma que espera el protocolo.
#[derive(Debug)]
pub struct AdminPending {
    pub fields_psv: Vec<PackStreamValue>,
    pub rows: Vec<Vec<PackStreamValue>>,
}

/// El error que devuelve cuando no: código y mensaje, tal cual viajan al
/// cliente.
pub type AdminFailure = (String, String);

/// Lo que el camino de consulta necesita saber de quien pide algo.
///
/// Se pasa agrupado en vez de como cuatro argumentos sueltos porque las dos
/// implementaciones necesitan exactamente lo mismo, y porque una firma con
/// cuatro valores del mismo tipo invita a intercambiarlos por descuido.
pub struct AdminCaller<'a> {
    /// Quién lo pide.
    pub username: &'a str,
    /// Si es administrador. Cada implementación decide qué exige: hay una
    /// excepción deliberada —consultar los permisos de uno mismo— que no la
    /// necesita.
    pub is_admin: bool,
    /// La conexión desde la que llega, para dejar constancia en el registro.
    pub connection_id: u64,
}

/// El punto de extensión: sirve una sentencia administrativa.
///
/// Cada edición monta su implementación al arrancar. El camino de consulta
/// entrega la sentencia sin saber cuál está montada.
#[async_trait::async_trait]
pub trait AdminDispatcher: Send + Sync {
    /// Sirve la sentencia, o devuelve el error que verá el cliente.
    ///
    /// # Errors
    ///
    /// Devuelve código y mensaje cuando quien llama no tiene privilegio, cuando
    /// la sentencia no es válida, o cuando **esta edición no ofrece esa
    /// función** — este último con un mensaje que lo dice, no con una respuesta
    /// vacía.
    async fn dispatch(
        &self,
        stmt: AdminStatement,
        caller: AdminCaller<'_>,
        audit: &AuditSink,
    ) -> Result<AdminPending, AdminFailure>;
}

/// Construye el despachador administrativo de una edición.
///
/// Misma forma que las demás factorías del arranque, y por el mismo motivo: el
/// montaje común llama a la que le hayan dado sin saber cuál es.
pub type AdminDispatcherFactory = Arc<dyn Fn() -> Arc<dyn AdminDispatcher> + Send + Sync>;

/// Cómo se construye el despachador de pago a partir del gestor concreto.
///
/// El árbol público no puede nombrar ese despachador, así que lo declara como
/// **un hueco**: una función que la edición de pago instala al montarse. Aquí
/// sólo se guarda y se llama.
pub type PaidDispatcherBuilder = Arc<dyn Fn() -> Arc<dyn AdminDispatcher> + Send + Sync>;

/// Elige el despachador de esta conexión.
///
/// Si quien monta la conexión trajo un constructor de pago, se usa. Si no, el
/// público — que sirve las seis de cuentas y falla cerrado en las demás.
///
/// **La decisión se toma al montar, no al servir una consulta**, y ése era el
/// punto: antes el camino de consulta preguntaba a un módulo de pago si la
/// sentencia era de pago y elegía entre dos despachadores. Ahora recibe uno ya
/// elegido y no sabe cuál es.
#[must_use]
pub fn build_dispatcher(
    paid: Option<&PaidDispatcherBuilder>,
    users: &Arc<dyn UserStore>,
) -> Arc<dyn AdminDispatcher> {
    paid.map_or_else(
        || Arc::new(CommunityAdminDispatcher::new(Arc::clone(users))) as Arc<dyn AdminDispatcher>,
        |build| build(),
    )
}

/// El despachador de la edición pública: sirve las seis sentencias de cuentas
/// locales y nada más.
///
/// Lleva la gestión de usuarios, que es la única superficie de identidad que
/// esta edición trae. Ni catálogo ni permisos: no los tiene, y por eso las
/// otras seis fallan cerradas.
pub struct CommunityAdminDispatcher {
    users: Arc<dyn UserStore>,
}

impl CommunityAdminDispatcher {
    #[must_use]
    pub fn new(users: Arc<dyn UserStore>) -> Self {
        Self { users }
    }
}

#[async_trait::async_trait]
impl AdminDispatcher for CommunityAdminDispatcher {
    async fn dispatch(
        &self,
        stmt: AdminStatement,
        caller: AdminCaller<'_>,
        audit: &AuditSink,
    ) -> Result<AdminPending, AdminFailure> {
        // Lo primero: si la sentencia no es de esta edición, decirlo. ANTES de
        // mirar privilegios, y el orden no es casual — responder "no eres
        // administrador" sobre una función que esta edición no tiene manda al
        // operador a buscar una credencial que no le va a servir. La verdad
        // útil es que la función no está.
        if !serves(&stmt) {
            return Err((
                "Neo.ClientError.General.UnknownError".to_owned(),
                "database and grant administration are not available in this edition".to_owned(),
            ));
        }
        crate::admin_users::dispatch_user_admin(
            stmt,
            &*self.users,
            caller.username,
            caller.is_admin,
            caller.connection_id,
            audit,
        )
        .await
    }
}

/// Si esta edición sirve la sentencia: las seis de cuentas sí, el resto no.
///
/// La lista vive aquí, junto a la implementación que la usa, y **no en el
/// camino de consulta**: ahí es donde estaba antes y era el problema. Al partir
/// el árbol, la edición pública se lleva esta lista —que describe lo que SÍ
/// hace— y no la de las sentencias de pago.
fn serves(stmt: &AdminStatement) -> bool {
    matches!(
        stmt,
        AdminStatement::CreateUser { .. }
            | AdminStatement::DropUser { .. }
            | AdminStatement::AlterUserPassword { .. }
            | AdminStatement::AlterUserStatus { .. }
            | AdminStatement::AlterUserAdmin { .. }
            | AdminStatement::ShowUsers
    )
}
