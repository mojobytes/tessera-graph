// SPDX-License-Identifier: BSL-1.1

//! La respuesta de la **edición pública** a los procedimientos que necesitan el
//! gestor concreto detrás: decir que no los ofrece.
//!
//! # Por qué existe este fichero
//!
//! El camino de consulta reconoce los nombres de los procedimientos de copia en
//! caliente —el catálogo de nombres vive en el motor, que es público— y tiene
//! que hacer algo con ellos. En la edición de pago los encamina hacia la vía de
//! copia; aquí no hay adónde encaminarlos.
//!
//! Estaba resuelto llamando directamente a la vía de copia desde el camino de
//! consulta. Como ese fichero viaja al árbol público, allí nombraba un método
//! que no existe, y el árbol público no compilaba. **Un nombre tiene que
//! resolverse aunque su rama no llegue a ejecutarse nunca**: no basta con que
//! la condición sea siempre falsa.
//!
//! Por eso son dos ficheros y no una condición dentro de uno. Es el mismo
//! reparto que ya tenían la raíz del gestor de bases y la del módulo de
//! migración: cada edición trae el suyo, y al copiar el árbol el público ocupa
//! el lugar del de pago.
//!
//! # Qué responde, y por qué así
//!
//! Un fallo que dice que esta edición no ofrece la función, no una respuesta
//! vacía ni un resultado de cero filas. Un cliente que pide una copia y recibe
//! "hecho, cero ficheros" se cree respaldado y no lo está; recibiendo un error
//! sabe que tiene que buscar otra vía. Es la misma decisión que gobierna el
//! despachador administrativo público, que falla cerrado en las sentencias de
//! catálogo y permisos en lugar de devolverlas vacías.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::auth::AuthProvider;
use crate::error::Result;
use crate::handler::BoltHandler;

impl<S, A: ?Sized> BoltHandler<S, A>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    A: AuthProvider,
{
    /// Responde que esta edición no ofrece los procedimientos que cuelgan del
    /// gestor concreto. **Edición pública.**
    ///
    /// Los dos argumentos se reciben y no se miran: la respuesta es la misma
    /// para instantánea que para restauración, y no depende de con qué
    /// argumentos se pidiera. Se conservan para que la firma sea idéntica a la
    /// de la edición de pago — es lo que permite que el camino de consulta
    /// llame igual en las dos sin saber cuál está sirviendo.
    pub(crate) async fn dispatch_registry_scoped_call(
        &mut self,
        _kind: Option<ermya_graph::call::ProcedureKind>,
        _stmt: &ermya_graph::gql::CallStatement,
    ) -> Result<bool> {
        self.fail_with(
            "Neo.ClientError.General.UnknownError",
            "backup procedures are not available on this server",
        )
        .await?;
        Ok(false)
    }
}
