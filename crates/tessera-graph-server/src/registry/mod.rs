// SPDX-License-Identifier: BSL-1.1

//! Raíz del módulo del gestor de bases en la **edición pública**.
//!
//! Declara la interfaz, el gestor de una sola base y los tipos compartidos. No
//! declara el gestor multi-base ni sus tareas de fondo: no viajan.
//!
//! El repositorio de pago tiene su propia raíz (`mod.rs`), que declara además
//! todo lo anterior. Son dos ficheros porque una raíz no se puede partir: o
//! nombra un módulo o no lo nombra.

mod graph_registry;
mod shared;
mod single;

pub use graph_registry::GraphRegistry;
pub use shared::{COMMUNITY_DATABASE, DbHandle, EngineLimits, RegistryError};
pub(crate) use shared::{MIN_SWEEP_INTERVAL, OPEN_TIMEOUT, open_graph_with_mvcc};
pub use single::SingleDatabaseManager;
