// SPDX-License-Identifier: BSL-1.1
//! Community single-database manager tests.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tessera_graph::{Graph, props};
use tessera_graph_server::auth::{AccessLevel, SystemGraphAuthStore, UserStore};
use tessera_graph_server::registry::{EngineLimits, GraphRegistry, SingleDatabaseManager};

/// Build the user store the Community manager needs.
///
/// The return type is the point of this helper: `Arc<dyn UserStore>`, the
/// **local user management** surface and nothing else. The Community edition
/// must not require grants or the multi-database catalogue — it has neither.
/// If this stops compiling because a wider identity surface is demanded
/// again, that is the regression, not a helper to widen.
fn build_test_users() -> Arc<dyn UserStore> {
    let g = Arc::new(RwLock::new(Graph::new()));
    Arc::new(SystemGraphAuthStore::new(g).expect("store"))
}

#[tokio::test]
async fn single_manager_opens_the_one_database_with_full_access() {
    let tmp = tempfile::tempdir().unwrap();
    let identity = build_test_users();
    let mgr = SingleDatabaseManager::new(
        identity,
        tmp.path().join("databases").join("graph"),
        "graph".to_string(),
        EngineLimits::default(),
    )
    .await
    .expect("build single manager");

    let h = mgr.acquire("graph", "admin").await.expect("acquire");
    assert_eq!(h.database_name(), "graph");
    assert_eq!(
        h.access_level(),
        AccessLevel::ReadWrite,
        "Community = acceso total"
    );

    // Una sola base: acquire de cualquier nombre sirve la única base.
    // El gestor Community NO valida el nombre contra un catálogo multi-DB.
    drop(h);
    mgr.close_all(Duration::from_secs(1)).await;
}

/// El asa reporta la base REALMENTE abierta, no el nombre pedido.
///
/// El gestor Community no usa el nombre como clave de búsqueda: sirve su
/// única base venga el nombre que venga. Quien registre el nombre pedido en
/// vez del abierto deja constancia de acceso a una base inexistente — y la
/// auditoría es justamente lo que reconstruye "quién tocó qué". Bajo el
/// gestor multi-inquilino los dos nombres coinciden siempre (el catálogo
/// rechaza cualquier otro), así que sin este test la divergencia no la
/// vigila nadie.
#[tokio::test]
async fn handle_reports_the_database_actually_opened_not_the_one_requested() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = SingleDatabaseManager::new(
        build_test_users(),
        tmp.path().join("databases").join("neo4j"),
        "neo4j".to_string(),
        EngineLimits::default(),
    )
    .await
    .expect("build single manager");

    let h = mgr
        .acquire("una-base-que-no-existe", "admin")
        .await
        .expect("Community sirve su única base sea cual sea el nombre pedido");

    assert_eq!(
        h.database_name(),
        "neo4j",
        "el asa debe nombrar la base abierta; devolver el nombre pedido \
         haría que la auditoría registrase una base inexistente"
    );

    drop(h);
    mgr.close_all(Duration::from_secs(1)).await;
}

/// `close_all` must leave the single database durable on disk.
///
/// The Enterprise registry drains and drops its entries on shutdown; the
/// Community manager owns its `Graph` for the life of the process, so the
/// journal is only consolidated if `close_all` checkpoints explicitly. The
/// engine has no `Drop` impl for `Graph` — nothing reclaims this implicitly —
/// and `Graph::flush`'s own docs call for a flush "on shutdown". Without it a
/// clean shutdown leaves the whole session in `wal.log`, which the next open
/// must replay (#58 measured 3.55 GB of journal against 22 minutes of startup).
#[tokio::test]
async fn close_all_checkpoints_the_single_database() {
    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("databases").join("graph");
    let mgr = SingleDatabaseManager::new(
        build_test_users(),
        db_dir.clone(),
        "graph".to_string(),
        EngineLimits::default(),
    )
    .await
    .expect("build single manager");

    // Write enough to make the journal unmistakably non-empty.
    {
        let h = mgr.acquire("graph", "admin").await.expect("acquire");
        let graph = h.graph();
        let mut g = graph.write().expect("write lock");
        for i in 0..500_u64 {
            g.add_node("Person", props! { "n" => i.to_string() })
                .expect("write succeeds");
        }
    }

    let wal = db_dir.join("wal.log");
    let before = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert!(
        before > 0,
        "precondición inválida: sin journal que consolidar, el test no vigila nada"
    );

    mgr.close_all(Duration::from_secs(5)).await;

    let after = std::fs::metadata(&wal).map_or(0, |m| m.len());
    assert_eq!(
        after, 0,
        "close_all debe consolidar el journal: {before} bytes antes, {after} después"
    );
}

/// El gestor Community recupera la memoria de las versiones antiguas.
///
/// Una transacción explícita que escribe y confirma deja versiones en memoria
/// hasta que alguien las materializa a la página. El motor tiene la operación
/// —es suya, y su contrato dice que no ejecutarla "sólo cuesta memoria"— pero
/// la tarea de fondo que la invoca vivía sólo en el gestor multi-base.
///
/// Las transacciones explícitas SON Community: el motor entero va en esa
/// edición. Sin esto, un servidor Community acumula esas versiones durante
/// toda la vida del proceso y nunca las libera, mientras el de pago sí lo
/// hace. No es reparto de funcionalidad: es una fuga de memoria en la edición
/// pública.
#[tokio::test]
async fn vacuum_once_reclaims_committed_version_memory() {
    use tessera_graph::Properties;

    let tmp = tempfile::tempdir().unwrap();
    let mgr = SingleDatabaseManager::new(
        build_test_users(),
        tmp.path().join("databases").join("neo4j"),
        "neo4j".to_string(),
        EngineLimits::default(),
    )
    .await
    .expect("build single manager");

    {
        let h = mgr.acquire("neo4j", "admin").await.expect("acquire");
        let graph = h.graph();
        let mut g = graph.write().expect("write lock");
        let txn = g.begin_txn().expect("begin");
        g.add_node_in_txn(txn, "N", Properties::new()).expect("write");
        g.commit_txn(txn).expect("commit");
    }

    // Sin transacción viva, toda cadena confirmada es recuperable.
    let freed = mgr.vacuum_once().await;
    assert!(
        freed >= 1,
        "el gestor Community debe recuperar la memoria de las versiones \
         confirmadas; recuperadas {freed}"
    );

    // Segunda pasada: ya no queda nada que recuperar. Sin esto, el test pasaría
    // igual con una implementación que contase mal.
    let again = mgr.vacuum_once().await;
    assert_eq!(
        again, 0,
        "nada que recuperar tras la primera pasada, recuperadas {again}"
    );

    mgr.close_all(Duration::from_secs(1)).await;
}
