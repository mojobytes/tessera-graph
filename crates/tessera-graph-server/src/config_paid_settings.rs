// SPDX-License-Identifier: BSL-1.1

//! La **forma** de los ajustes del gestor multi-base.
//!
//! # Por qué la forma viaja y la factoría no
//!
//! Los ajustes salieron de la configuración común a su propia factoría
//! (apartado 5.10): el árbol público ya no los lee ni decide nada sobre ellos.
//! Pero la estructura común sigue teniendo que **transportarlos**, y para eso
//! necesita nombrar su tipo.
//!
//! Así que se parte en dos, con el mismo criterio de siempre:
//!
//! - **La forma** —qué campos hay y de qué tipo— viaja al árbol público, porque
//!   sin ella la configuración común no compila. Es una declaración inerte: no
//!   lee nada del entorno ni decide nada.
//! - **La factoría que los analiza** se queda en la edición de pago. Es la que
//!   convierte variables de entorno en valores, y la que sabe qué nombres
//!   buscar.
//!
//! En la edición pública la estructura llega siempre con sus valores por
//! defecto y nadie la mira: no hay gestor multi-base al que gobernar.

/// Los siete ajustes del gestor multi-base.
///
/// Todos tienen un valor por defecto utilizable, así que un despliegue de pago
/// que no configure nada arranca igual: la factoría rellena lo que falte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaidSettings {
    /// Cuánto tiempo se conserva abierta una base que nadie usa antes de
    /// cerrarla. En cero no se cierra ninguna: se mantienen abiertas hasta
    /// apagar el servidor. Por defecto 900 segundos.
    pub idle_ttl_seconds: u64,
    /// Tope de conexiones simultáneas que se aplica a una base creada sin tope
    /// propio. Por defecto 100.
    pub default_max_connections: usize,
    /// Tope de tamaño que se aplica a una base creada sin tope propio. Sin
    /// valor significa sin tope, que es lo predeterminado.
    pub default_max_size_bytes: Option<u64>,
    /// Cada cuánto se despierta la tarea que cierra las bases inactivas. Por
    /// defecto 60 segundos.
    pub registry_sweep_interval_seconds: u64,
    /// A partir de cuántas bases retenidas más allá de su tiempo de inactividad
    /// conviene avisar en el registro. Por defecto 50.
    pub ttl_disabled_warn_threshold: usize,
    /// Tope global de bases abiertas a la vez. Sin valor significa sin tope.
    /// Al alcanzarlo, abrir una base nueva falla de forma transitoria: el
    /// operador sube el tope o espera a que se libere una por inactividad.
    pub max_open_databases: Option<usize>,
    /// Cada cuánto se sondean las medidas por base. No se lee del entorno: es
    /// un detalle interno, y su valor por defecto concuerda con la cadencia de
    /// recogida documentada.
    pub metrics_poll_interval: std::time::Duration,
}

impl Default for PaidSettings {
    fn default() -> Self {
        Self {
            idle_ttl_seconds: 900,
            default_max_connections: 100,
            default_max_size_bytes: None,
            registry_sweep_interval_seconds: 60,
            ttl_disabled_warn_threshold: 50,
            max_open_databases: None,
            metrics_poll_interval: std::time::Duration::from_secs(15),
        }
    }
}

