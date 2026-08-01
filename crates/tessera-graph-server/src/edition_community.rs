// SPDX-License-Identifier: BSL-1.1

//! Lo que el binario necesita saber de su edición. **Edición pública.**
//!
//! # Por qué existe
//!
//! El punto de arranque del binario tocaba la edición en dos sitios: la puerta
//! de arranque y la lectura de los ajustes del gestor multi-base. Dos líneas en
//! un fichero de 138 que, por lo demás, es idéntico en las dos ediciones.
//!
//! Duplicar el fichero entero por dos líneas se descartó: dos copias largas se
//! separan con el tiempo y la que menos se toca se queda atrás sin que nadie lo
//! note. Se duplica **sólo lo que difiere** — este fichero — y el binario llama
//! aquí sin saber qué edición le responde.
//!
//! **Viaja con otro nombre**, ocupando el lugar de `edition_enterprise.rs`: los
//! dos declaran las mismas dos funciones y sólo una puede existir a la vez.
//!
//! # Qué cambia respecto a la edición de pago
//!
//! Las dos cosas que esta edición no tiene:
//!
//! 1. **El arranque monta el gestor de una sola base.** Por la puerta neutra,
//!    pasándole la factoría pública. La puerta cableada al gestor multi-base se
//!    queda en la otra edición junto con lo que cablea.
//! 2. **No se leen los ajustes del gestor multi-base.** Quedan en sus valores
//!    por defecto, que es lo correcto cuando no hay gestor que gobernar:
//!    expulsión por inactividad, número máximo de bases abiertas y demás sólo
//!    significan algo teniendo varias bases que abrir y cerrar.
//!
//!    Leerlos y no aplicarlos sería peor que no leerlos: un ajuste aceptado y
//!    desatendido es cómo un operador acaba creyendo que puso un tope que no
//!    está puesto. Aquí ni se leen, así que no hay nada que creer.

use std::collections::HashMap;
use std::hash::BuildHasher;

use tokio::sync::watch;

use crate::config::ServerConfig;
use crate::config_paid_settings::PaidSettings;
use crate::error::Result;
use crate::startup::{PaidStartupHooks, ServerHandle, single_database_factory};

/// Los ajustes del gestor multi-base: los de por defecto, porque esta edición
/// no tiene gestor multi-base.
///
/// Se reciben las variables de entorno y no se miran. La firma es idéntica a la
/// de la edición de pago para que el binario llame igual en las dos.
#[must_use]
pub fn parse_paid_settings<S: BuildHasher>(_vars: &HashMap<String, String, S>) -> PaidSettings {
    PaidSettings::default()
}

/// Arranca el servidor de esta edición: el de una sola base.
///
/// Por la puerta neutra, con la factoría pública y sin enganches de pago — no
/// hay bases que medir, ni catálogo que administrar, ni restauraciones a medias
/// que reponer.
///
/// # Errors
///
/// Falla si no se puede abrir el puerto, si el grafo de sistema no abre, si los
/// permisos de su directorio son demasiado abiertos, si otro proceso ya lo tiene
/// cogido, o si el cifrado está mal configurado.
pub async fn start_server(
    config: ServerConfig,
    shutdown: watch::Receiver<bool>,
) -> Result<ServerHandle> {
    crate::startup::start_server_with_registry(
        config,
        shutdown,
        None,
        single_database_factory(),
        PaidStartupHooks::default(),
    )
    .await
}
