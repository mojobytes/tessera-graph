// SPDX-License-Identifier: BSL-1.1

//! Raíz del módulo de versionado del formato en disco en la **edición
//! pública**.
//!
//! Declara el guardián de la versión y nada más. No declara el plan de
//! migración de una base a varias: sembrar el catálogo y repartir permisos son
//! cosas que esta edición no tiene, así que el plan no viaja.
//!
//! El repositorio de pago tiene su propia raíz (`mod.rs`), que declara además
//! el plan y su primer paso. Son dos ficheros porque **una raíz no se puede
//! partir**: o nombra un módulo o no lo nombra. Es el mismo reparto que ya
//! tenían la raíz del gestor de bases y la vía de copia del manejador de
//! sesión.
//!
//! # Qué conserva la edición pública, y por qué importa
//!
//! El guardián entero. Comprueba al arrancar que el formato en disco es el que
//! el binario entiende y se niega a tocar nada si no coincide. Sin él, un
//! binario nuevo abriría datos escritos con un formato viejo y los corrompería
//! en silencio — y eso le pasa igual a un servidor de una sola base que a uno
//! de cien.
//!
//! El módulo entero figuraba como de pago. Lo destapó compilar el árbol público
//! por primera vez: el montaje común llamaba al guardián en tres sitios y el
//! módulo no viajaba. La clasificación estaba mal, no el código.

/// El guardián de la versión del formato en disco.
pub mod layout;

pub use layout::{
    CURRENT_DISK_LAYOUT, MigrationError, SchemaVersion, read_or_reject, write, write_if_missing,
};
