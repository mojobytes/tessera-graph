// SPDX-License-Identifier: BSL-1.1

//! El hueco por el que una sesión transporta el gestor de pago.
//!
//! # Por qué es su propio fichero
//!
//! El manejador de sesión y el bucle de aceptación **transportan** este valor
//! —lo reciben al montar y se lo pasan a las vías de pago— pero no lo usan. Los
//! dos viajan al árbol público, así que el alias que lo nombra tiene que viajar
//! con ellos.
//!
//! Estaba definido dentro del módulo de pago. El resultado: dos ficheros
//! públicos importando de un módulo que no viaja, y un árbol público que no
//! compila. Es el mismo patrón que apareció con los tipos del gestor y con los
//! ajustes — **lo compartido no puede vivir en el lado que no se copia**.
//!
//! # Qué es en cada edición
//!
//! En la de pago, el gestor multi-base. En la pública, un hueco que nadie puede
//! llenar: el tipo que lo rellenaría no existe allí, y eso es exactamente la
//! afirmación que queremos — un servidor de una sola base **no tiene** catálogo.

use std::sync::Arc;

/// El gestor de pago tal y como lo transporta una sesión.
///
/// Vacío es el montaje público: sin catálogo, sin permisos, sin copia en
/// caliente.
pub type MultiTenantHandle = Option<Arc<PaidRegistry>>;

/// El gestor de pago, cuando ya se sabe que lo hay.
pub type PaidRegistryRef = Arc<PaidRegistry>;

/// El gestor de pago tal y como lo transporta el **montaje**.
///
/// Mismo tipo que [`PaidRegistryRef`], distinto nombre porque se lee en otro
/// sitio: las tres estructuras del arranque lo llevan de un lado a otro —la
/// factoría lo produce, el montaje lo pasa, el asa lo entrega— sin necesitar
/// saber qué es.
///
/// Estaba definido en el módulo de arranque de pago, que no viaja, y lo
/// nombraba el arranque común, que sí. Es el mismo patrón por novena vez: **lo
/// compartido no puede vivir en el lado que no se copia**.
pub type StartupPaidRegistry = Arc<PaidRegistry>;

/// El tipo concreto del gestor de pago.
///
/// **Es esta línea, y sólo esta, la que cambia entre ediciones**, y cambia
/// porque el fichero se copia distinto — no por un interruptor de compilación.
/// Un interruptor dejaría el nombre del gestor de pago escrito en el árbol
/// público aunque la rama no se compilara, que es justo lo que la mudanza evita.
///
/// En el repositorio de pago apunta al gestor multi-base. Al copiar, el guion lo
/// sustituye por un tipo sin habitantes: el hueco deja de poder llenarse, que es
/// la afirmación correcta para un servidor de una sola base.
pub type PaidRegistry = std::convert::Infallible;
