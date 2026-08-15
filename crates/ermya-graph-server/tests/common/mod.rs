// SPDX-License-Identifier: BSL-1.1

//! Shared test helpers for `ermya-graph-server` integration tests.

use std::sync::Arc;
use std::time::Duration;

use ermya_graph::Graph;
use ermya_graph_protocol::PackStreamValue;
use ermya_graph_protocol::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
use ermya_graph_protocol::bolt_message::{BoltRequest, BoltResponse};
use ermya_graph_protocol::{BOLT_MAGIC, decode_response, encode_request};
use ermya_graph_server::BoltHandler;
use ermya_graph_server::audit::AuditSink;
use ermya_graph_server::auth::{AuthProvider, SystemGraphAuthStore, UserStore};
use ermya_graph_server::graph_accessor::GraphAccessor;
use ermya_graph_server::registry::GraphRegistry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Monta un manejador con el tope de filas del resultado en
/// `max_result_rows`. `0` lo desactiva; cualquier valor positivo aborta las
/// consultas cuyo conjunto de coincidencias o cuya salida lo superen. El
/// resto del montaje es el neutro: sin almacén de identidad, sin auditoría,
/// 30 s de inactividad y sin aviso de consulta lenta.
#[allow(dead_code)]
pub async fn spawn_bolt_handler_with_cap<A>(
    auth: Arc<A>,
    // **La interfaz, no el tipo concreto**: el tope de filas es neutro y sus
    // pruebas viajan al árbol público.
    registry: Arc<dyn GraphRegistry>,
    max_result_rows: u64,
) -> (
    BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::sync::watch::Sender<bool>,
)
where
    A: AuthProvider + 'static,
{
    let (client_stream, server_stream) = tokio::io::duplex(65_536);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let query_cache = Arc::new(ermya_graph_cypher::cache::QueryCache::new(256));

    tokio::spawn(async move {
        match BoltHandler::new_with_handshake(
            server_stream,
            auth,
            default_auth_store(),
            AuditSink::off(),
            registry,
            None, // sin gestor concreto: montaje neutro
            None, // sin despachador de pago
            query_cache,
            Duration::from_secs(30),
            0,
            0,
            max_result_rows,
            0,
            0,
            0, // query_timeout_ms (Task 6 — disabled in tests)
            format!("Neo4j/{}", env!("CARGO_PKG_VERSION")), // server_agent (Block 1)
            None,
            None,
            shutdown_rx,
        )
        .await
        {
            Ok(mut handler) => {
                let _ = handler.run().await;
            }
            Err(e) => {
                eprintln!("bolt handler error: {e}");
            }
        }
    });

    let (mut client_read, mut client_write) = tokio::io::split(client_stream);

    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    client_write.write_all(&handshake).await.unwrap();
    client_write.flush().await.unwrap();

    let mut resp = [0u8; 4];
    client_read.read_exact(&mut resp).await.unwrap();
    assert_eq!(
        resp,
        [0x00, 0x00, 0x04, 0x04],
        "bolt handshake version mismatch"
    );

    (
        BoltChunkedWriter::new(client_write),
        BoltChunkedReader::new(client_read),
        shutdown_tx,
    )
}

/// v0.6.0 Task 5 eje 2 — spawn a handler whose per-connection query
/// token bucket is capped at `queries_max_per_second` (capacity =
/// `cap * 2`, refill = `cap`/sec). Uses `NoAuthProvider` so a single
/// HELLO authenticates and RUN/PULL/DISCARD dispatch through the legacy
/// fallback graph (registry `None`). Audit events go to a file so the
/// test can assert the `query_throttled` event shape. Returns the same
/// six-tuple as [`HandlerWithAudit`].
#[allow(dead_code)]
pub async fn spawn_bolt_handler_with_query_cap(queries_max_per_second: u32) -> HandlerWithAudit {
    spawn_bolt_handler_no_auth_with_caps(queries_max_per_second, 0).await
}

/// v0.6.0 Task 5 eje 4 — spawn a `NoAuthProvider` handler whose
/// per-connection bandwidth cap is `max_bytes_per_second` (capacity =
/// `cap * 2`, refill = `cap`/sec). Audit events go to a file so the
/// timing test can also assert the aggregate `bandwidth_throttled`
/// event emitted on connection close. Returns a [`HandlerWithAudit`].
#[allow(dead_code)]
pub async fn spawn_bolt_handler_with_bytes_cap(max_bytes_per_second: u64) -> HandlerWithAudit {
    spawn_bolt_handler_no_auth_with_caps(0, max_bytes_per_second).await
}

/// Shared spawner for the eje-2 (query) and eje-4 (bytes) integration
/// tests: a `NoAuthProvider` handler on a duplex stream, file-backed
/// audit, a registry holding [`DEFAULT_TEST_DB`] granted to `anonymous`
/// (the principal `NoAuthProvider` authenticates), with the query and
/// bandwidth caps configurable. Avoids duplicating the handshake dance.
async fn spawn_bolt_handler_no_auth_with_caps(
    queries_max_per_second: u32,
    max_bytes_per_second: u64,
) -> HandlerWithAudit {
    spawn_bolt_handler_no_auth_full(queries_max_per_second, max_bytes_per_second, 0).await
}

/// Generalised `NoAuthProvider` + file-backed-audit spawner sobre el gestor de
/// una sola base. La base de datos y el registro de auditoría comparten un
/// único directorio temporal (el registro en `<tmp>/audit.log`, los datos en
/// `<tmp>/databases`), así que la única ranura de directorio temporal del
/// séxtuplo mantiene vivo todo el estado en disco. Expone los topes de
/// consultas y de tráfico, más el tiempo máximo de consulta.
///
/// Los topes que estas pruebas ejercen son por conexión y los aplica el
/// manejador de sesión antes de tocar ningún grafo, así que el gestor de una
/// sola base ejerce exactamente el mismo camino que el multi-base.
#[allow(dead_code)]
async fn spawn_bolt_handler_no_auth_full(
    queries_max_per_second: u32,
    max_bytes_per_second: u64,
    query_timeout_ms: u64,
) -> HandlerWithAudit {
    use ermya_graph_server::auth::NoAuthProvider;
    use ermya_graph_server::registry::{COMMUNITY_DATABASE, EngineLimits, SingleDatabaseManager};

    let tmp = tempfile::TempDir::new().expect("tempdir for capped handler");
    let store = Arc::new(
        SystemGraphAuthStore::new(Arc::new(std::sync::RwLock::new(Graph::new())))
            .expect("system graph store"),
    );
    let auth_store: Arc<dyn UserStore> = store;
    let manager = SingleDatabaseManager::new(
        Arc::clone(&auth_store),
        tmp.path().join("databases").join(COMMUNITY_DATABASE),
        COMMUNITY_DATABASE.to_owned(),
        EngineLimits::default(),
    )
    .await
    .expect("build community manager");
    let registry = Arc::new(manager) as Arc<dyn GraphRegistry>;

    let audit_path = tmp.path().join("audit.log");
    let (audit_shutdown_tx, audit_shutdown_rx) = tokio::sync::watch::channel(false);
    let audit = AuditSink::file(audit_path.clone(), 1_000_000, 3, 0, audit_shutdown_rx)
        .expect("audit sink");

    let (client_stream, server_stream) = tokio::io::duplex(65_536);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let query_cache = Arc::new(ermya_graph_cypher::cache::QueryCache::new(256));

    tokio::spawn(async move {
        match BoltHandler::new_with_handshake(
            server_stream,
            Arc::new(NoAuthProvider),
            auth_store,
            audit,
            registry,
            // Sin gestor de pago y sin despachador de pago: es el montaje
            // público, y el hueco no puede llenarse aquí ni queriendo.
            None,
            None,
            query_cache,
            Duration::from_secs(30),
            0,
            0,
            0,
            queries_max_per_second,
            max_bytes_per_second,
            query_timeout_ms,
            format!("Neo4j/{}", env!("CARGO_PKG_VERSION")), // server_agent (Block 1)
            None,
            None,
            shutdown_rx,
        )
        .await
        {
            Ok(mut handler) => {
                let _ = handler.run().await;
            }
            Err(e) => {
                eprintln!("bolt handler error: {e}");
            }
        }
    });

    let (mut client_read, mut client_write) = tokio::io::split(client_stream);
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    client_write.write_all(&handshake).await.unwrap();
    client_write.flush().await.unwrap();
    let mut resp = [0u8; 4];
    client_read.read_exact(&mut resp).await.unwrap();
    assert_eq!(
        resp,
        [0x00, 0x00, 0x04, 0x04],
        "bolt handshake version mismatch"
    );

    (
        BoltChunkedWriter::new(client_write),
        BoltChunkedReader::new(client_read),
        shutdown_tx,
        audit_shutdown_tx,
        tmp,
        audit_path,
    )
}

/// Create a default graph wrapped for the `DefaultGraphAccessor`.
#[allow(dead_code)]
pub fn default_graph() -> Arc<ermya_graph_server::DefaultGraphAccessor> {
    use std::sync::RwLock;
    let graph = Arc::new(RwLock::new(Graph::new()));
    Arc::new(ermya_graph_server::DefaultGraphAccessor::new(graph))
}

/// The default database every legacy-style spawn helper now binds to. Tests
/// that previously ran against the single-graph fallback send their RUNs with
/// `extra["db"] = DEFAULT_TEST_DB` (see [`run_message`]).
#[allow(dead_code)]
pub const DEFAULT_TEST_DB: &str = "testdb";

/// Build a registry backed by a `TempDir`, holding a single database
/// [`DEFAULT_TEST_DB`] granted `ReadWrite` to every user the test's auth
/// provider will authenticate. Replaces the removed single-graph fallback:
/// legacy spawn helpers mount this so a RUN carrying `extra["db"] =
/// DEFAULT_TEST_DB` resolves to a real per-tenant graph. The `TempDir` is
/// returned so the caller keeps the on-disk state alive.
///
/// `grant_users` are granted `ReadWrite` on the database so whichever principal
/// a test authenticates (via its own `AuthProvider`) can also bind the DB. Pass
/// the principals the test's HELLO will use.
#[allow(dead_code)]
pub async fn bolt_send(
    writer: &mut BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    req: &BoltRequest,
) {
    let data = encode_request(req).unwrap();
    writer.write_message(&data).await.unwrap();
}

/// Read a [`BoltResponse`] from the chunked reader.
#[allow(dead_code)]
pub async fn bolt_recv(
    reader: &mut BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
) -> BoltResponse {
    let data = reader
        .read_message()
        .await
        .unwrap()
        .expect("expected message");
    decode_response(&data).unwrap()
}

/// Almacén en memoria para las pruebas que no tocan cuentas.
///
/// Devuelve sólo gestión de usuarios: es lo que piden el manejador de sesión
/// y el gestor de una sola base, y lo único que esta edición implementa.
#[allow(dead_code)]
pub fn default_auth_store() -> Arc<dyn UserStore> {
    let g = Arc::new(std::sync::RwLock::new(Graph::new()));
    Arc::new(SystemGraphAuthStore::new(g).expect("auth store new"))
}

/// Deja `{data_dir}/system/` con una cuenta de administrador y la marca de
/// formato en disco al día, para que el arranque abra el directorio de datos
/// sin tener que crearlo desde cero.
///
/// Montaje necesario:
/// - `system/` con permisos `0o700`, que es lo que el arranque exige.
/// - Administrador con la contraseña indicada. El almacén impone un mínimo de
///   ocho caracteres, así que quien llame debe respetarlo.
/// - La marca de formato en disco, para que el guardián de migraciones acepte
///   el directorio como actualizado.
///
/// **Sin catálogo de bases.** Esta edición sirve una sola base, cuyo nombre es
/// fijo, y el gestor la abre él mismo al arrancar: no hay nada que registrar
/// por adelantado. La versión de pago de este ayudante sí puebla el catálogo.
///
/// El almacén y el grafo se sueltan antes de volver, para que el arranque pueda
/// tomar el cerrojo del grafo de sistema sin encontrárselo cogido.
///
/// Sólo para Unix, porque los permisos se imponen con la interfaz de permisos
/// de Unix.
#[cfg(unix)]
#[allow(dead_code)]
pub async fn prepopulate_system(data_dir: &std::path::Path, admin_password: &str) {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::RwLock as StdRwLock;
    use ermya_graph::GraphConfig;
    use ermya_graph_server::auth::SecretString;
    use ermya_graph_server::migration::{CURRENT_DISK_LAYOUT, write};

    let system_dir = data_dir.join("system");
    std::fs::create_dir_all(&system_dir).expect("create system dir");
    std::fs::set_permissions(&system_dir, std::fs::Permissions::from_mode(0o700))
        .expect("set system dir perms 0o700");
    {
        let graph = Graph::open(&system_dir, &GraphConfig::default())
            .expect("open system graph for pre-populate");
        let store =
            SystemGraphAuthStore::new(Arc::new(StdRwLock::new(graph))).expect("system auth store");
        store
            .create_user("admin", &SecretString::new(admin_password.to_owned()), true)
            .await
            .expect("create admin");
        // Se sueltan el almacén y el grafo para que el arranque pueda volver a
        // tomar el cerrojo del grafo de sistema.
    }
    write(data_dir, CURRENT_DISK_LAYOUT).expect("stamp ermya.version marker");
}

// ── Bolt request builders ───────────────────────────────────────────────────

/// Build a HELLO request with arbitrary string `extras` entries. The
/// caller supplies all entries — pass `principal`, `credentials`, and
/// optionally `database`. Tests of the multi-database HELLO flow rely
/// on the ability to omit `database` entirely (registry-mode rejection).
#[allow(dead_code)]
pub fn hello_with_extras(pairs: &[(&str, &str)]) -> BoltRequest {
    BoltRequest::Hello {
        extra: pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), PackStreamValue::String((*v).to_owned())))
            .collect(),
    }
}

/// Build a RUN request for `query` with no params and **no `db`** in the
/// extras. Kept deliberately db-less so protocol tests (RUN-before-HELLO,
/// missing-db rejection) still exercise the pre-bind path. Tests that need a
/// query to actually execute against the default test database must use
/// [`run_message_with_db`]`(query, DEFAULT_TEST_DB)` — the single-graph
/// fallback that used to serve a db-less RUN was removed in Plan B.
#[allow(dead_code)]
pub fn run_message(query: &str) -> BoltRequest {
    BoltRequest::Run {
        query: query.to_owned(),
        params: vec![],
        extra: vec![],
    }
}

/// Build a RUN request for `query` with `extra["db"] = db`. Used by
/// Task 10-bis tests where multi-database routing happens at first
/// RUN (Bolt 4.x/5.x contract) instead of HELLO.
///
/// The driver-facing `metadata.db` key on `RUN` and `BEGIN` is the
/// canonical place to carry the target database in Bolt — every
/// official Neo4j driver (Python, .NET, JS, Java, Go) emits it here.
/// The server's `handle_run` lazy-binds `DbHandle` to the session on
/// the first RUN that carries this key.
#[allow(dead_code)]
pub fn run_message_with_db(query: &str, db: &str) -> BoltRequest {
    BoltRequest::Run {
        query: query.to_owned(),
        params: vec![],
        extra: vec![("db".to_owned(), PackStreamValue::String(db.to_owned()))],
    }
}

/// Build a PULL request. Promoted from `handler_test.rs` so registry
/// helpers can drive RUN/PULL pipelines without re-importing it.
#[allow(dead_code)]
#[must_use]
pub const fn pull() -> BoltRequest {
    BoltRequest::Pull { extra: vec![] }
}

pub type HandlerWithAudit = (
    BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::sync::watch::Sender<bool>,
    tokio::sync::watch::Sender<bool>,
    tempfile::TempDir,
    std::path::PathBuf,
);

/// Build a registry-backed handler with `db_name` registered AND a
/// grant of `level` for `username`, plus a file-backed `AuditSink`
/// whose path is returned for later inspection. The `TempDir` keeps
/// the on-disk state alive; the audit log lives at
/// `<tmp>/audit.log`.
#[allow(dead_code)]
pub async fn fresh_community_handler_with_audit_file(username: &str) -> HandlerWithAudit {
    fresh_community_handler_with_audit_file_as(username, false).await
}

/// Variante que decide si el usuario creado es administrador.
///
/// Las sentencias administrativas de cuentas exigen ese distintivo, así que un
/// test que las ejerza necesita pedirlo. El valor por defecto sigue siendo
/// `false`: la mayoría de tests sólo necesita autenticarse.
#[allow(dead_code)]
pub async fn fresh_community_handler_with_audit_file_as(
    username: &str,
    is_admin: bool,
) -> HandlerWithAudit {
    use ermya_graph_server::auth::{SecretString, SystemGraphAuthProvider};
    use ermya_graph_server::registry::{COMMUNITY_DATABASE, EngineLimits, SingleDatabaseManager};

    let tmp = tempfile::TempDir::new().expect("tempdir for community handler");
    let audit_path = tmp.path().join("audit.log");
    let (audit_shutdown_tx, audit_shutdown_rx) = tokio::sync::watch::channel(false);
    let audit = AuditSink::file(audit_path.clone(), 1_000_000, 3, 0, audit_shutdown_rx)
        .expect("audit sink");

    // The identity backend lives in its own in-memory system graph, exactly
    // as the Community startup factory wires it.
    let store = Arc::new(
        SystemGraphAuthStore::new(Arc::new(std::sync::RwLock::new(Graph::new())))
            .expect("system graph store"),
    );
    // Sin sembrar nada de permisos: esta edición no los tiene, y el almacén ya
    // no ofrece esa siembra. Era lo que la decisión del reparto perseguía —un
    // servidor público no debe escribir en su grafo de sistema un nodo que su
    // edición nunca lee.
    // Same credential `send_ok_hello` drives with.
    store
        .create_user(username, &SecretString::new("passw0rd12".into()), is_admin)
        .await
        .expect("create user");

    // Tanto el gestor de una sola base como el manejador de sesión piden
    // únicamente gestión de usuarios. Pedirles la identidad completa —que
    // además incluye catálogo y permisos— ataría este andamio a una edición
    // que no es la suya.
    let manager = SingleDatabaseManager::new(
        Arc::clone(&store) as Arc<dyn UserStore>,
        tmp.path().join("databases").join(COMMUNITY_DATABASE),
        COMMUNITY_DATABASE.to_owned(),
        EngineLimits::default(),
    )
    .await
    .expect("build community manager");

    let auth = Arc::new(SystemGraphAuthProvider::from_store(Arc::clone(&store)));
    let auth_store: Arc<dyn UserStore> = store;
    let (writer, reader, shutdown) =
        spawn_bolt_handler_with_community_manager(auth, auth_store, audit, Arc::new(manager)).await;
    (writer, reader, shutdown, audit_shutdown_tx, tmp, audit_path)
}

/// Spawn a handler bound to an arbitrary [`GraphRegistry`] with no concrete
/// multi-tenant manager — the Community wiring.
#[allow(dead_code)]
async fn spawn_bolt_handler_with_community_manager<A>(
    auth: Arc<A>,
    auth_store: Arc<dyn UserStore>,
    audit: AuditSink,
    registry: Arc<dyn GraphRegistry>,
) -> (
    BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::sync::watch::Sender<bool>,
)
where
    A: AuthProvider + 'static,
{
    let (client_stream, server_stream) = tokio::io::duplex(65_536);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let query_cache = Arc::new(ermya_graph_cypher::cache::QueryCache::new(256));

    tokio::spawn(async move {
        match BoltHandler::new_with_handshake(
            server_stream,
            auth,
            auth_store,
            audit,
            registry,
            None, // Community: no concrete multi-tenant manager
            None, // paid_admin: sin gestor no hay despachador de pago
            query_cache,
            Duration::from_secs(30),
            0,
            0,
            0,
            0,
            0,
            0,
            format!("Neo4j/{}", env!("CARGO_PKG_VERSION")),
            None,
            None,
            shutdown_rx,
        )
        .await
        {
            Ok(mut handler) => {
                let _ = handler.run().await;
            }
            Err(e) => eprintln!("bolt handler error: {e}"),
        }
    });

    // Client side: same 20-byte Bolt 4.4 handshake the other spawners send.
    let (mut client_read, mut client_write) = tokio::io::split(client_stream);
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    client_write.write_all(&handshake).await.unwrap();
    client_write.flush().await.unwrap();

    let mut resp = [0u8; 4];
    client_read.read_exact(&mut resp).await.unwrap();
    assert_eq!(
        resp,
        [0x00, 0x00, 0x04, 0x04],
        "bolt handshake version mismatch"
    );

    (
        BoltChunkedWriter::new(client_write),
        BoltChunkedReader::new(client_read),
        shutdown_tx,
    )
}

/// Drain the audit writer and read every JSON line from
/// `audit_path`. The writer's `BufWriter` only fsyncs every
/// `fsync_every` events (`0` = never on the success path) or when the
/// sink's channel closes; tests use `fsync_every: 0` for performance,
/// so we send shutdown via `audit_tx` and wait for the writer to
/// drain `try_recv` + flush before reading.
#[allow(dead_code)]
pub async fn read_audit_events(
    audit_tx: &tokio::sync::watch::Sender<bool>,
    audit_path: &std::path::Path,
) -> Vec<serde_json::Value> {
    let _ = audit_tx.send(true);
    // Yield long enough for the writer task to react to shutdown,
    // drain pending mpsc messages, and call flush_and_sync.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let contents = std::fs::read_to_string(audit_path).unwrap_or_default();
    contents
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("valid JSON per line"))
        .collect()
}

/// Variant of [`fresh_handler_with_db_and_grant`] that also exposes
/// the shared [`DatabaseRegistry`] so tests can inspect the database
/// state through a second `acquire` call. Used by tenant-isolation
/// tests that need to verify a write actually landed in the
/// per-tenant graph and not in the legacy fallback accessor.
#[allow(dead_code)]
pub async fn send_ok_hello(
    writer: &mut BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    reader: &mut BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    username: &str,
    db_name: &str,
) {
    // 1. HELLO without `database` — pure authentication.
    bolt_send(
        writer,
        &hello_with_extras(&[("principal", username), ("credentials", "passw0rd12")]),
    )
    .await;
    let resp = bolt_recv(reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for HELLO {username}, got {resp:?}"
    );

    // 2. Binding RUN — first RUN carries `extra["db"]`. Use a trivial
    //    MATCH so we don't depend on any data being present.
    bolt_send(
        writer,
        &run_message_with_db("MATCH (n) RETURN count(n) AS c", db_name),
    )
    .await;
    let resp = bolt_recv(reader).await;
    assert!(
        matches!(resp, BoltResponse::Success { .. }),
        "expected SUCCESS for binding RUN against {db_name}, got {resp:?}"
    );

    // 3. Drain the PULL so the session goes back to Ready before the
    //    test's RUN of interest. One RECORD (the count) + SUCCESS.
    bolt_send(writer, &pull()).await;
    loop {
        match bolt_recv(reader).await {
            BoltResponse::Record { .. } => {}
            BoltResponse::Success { .. } => break,
            other => panic!("unexpected response while draining binding PULL: {other:?}"),
        }
    }
}

#[allow(dead_code)]
pub struct ListenerComponents {
    pub auth: Arc<dyn AuthProvider>,
    pub auth_store: Arc<dyn UserStore>,
    pub registry: Arc<dyn GraphRegistry>,
    pub tmp: tempfile::TempDir,
}

/// Monta los argumentos que `serve_plain` / `serve_tls` necesitan, de modo
/// que un saludo seguido de una
/// primera consulta llegue a un grafo real.
///
/// Uses `NoAuthProvider` (matches the legacy listener-test contract:
/// "TCP plain → handler responds"). El usuario `admin` se crea en el grafo de
/// sistema porque el manejador consulta el almacén, no el proveedor, para saber
/// si quien entra es administrador.
///
/// **El gestor es el de una sola base**, que es lo que esta edición ofrece: el
/// oyente recibe cualquier gestor por su interfaz y no distingue cuál le dan,
/// así que estas pruebas ejercen exactamente el mismo camino de red. El nombre
/// de base que se le pasa se ignora — hay una sola y siempre es la misma.
#[allow(dead_code)]
pub async fn fresh_listener_components(_db_name: &str) -> ListenerComponents {
    use std::sync::RwLock as StdRwLock;
    use tempfile::TempDir;
    use ermya_graph::GraphConfig;
    use ermya_graph_server::auth::{NoAuthProvider, SecretString};
    use ermya_graph_server::registry::{COMMUNITY_DATABASE, EngineLimits, SingleDatabaseManager};

    let tmp = TempDir::new().expect("tempdir for listener");
    let data_dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(data_dir.join("databases")).unwrap();
    let system_dir = data_dir.join("system");
    std::fs::create_dir_all(&system_dir).unwrap();

    let system_graph = ermya_graph::Graph::open(&system_dir, &GraphConfig::default())
        .expect("open system graph");
    let store = Arc::new(
        SystemGraphAuthStore::new(Arc::new(StdRwLock::new(system_graph)))
            .expect("system auth store"),
    );
    store
        .create_user(
            "admin",
            &SecretString::new("not-checked-12chars".to_owned()),
            true,
        )
        .await
        .expect("create admin");

    let auth_store: Arc<dyn UserStore> = store;

    let manager = SingleDatabaseManager::new(
        Arc::clone(&auth_store),
        data_dir.join("databases").join(COMMUNITY_DATABASE),
        COMMUNITY_DATABASE.to_owned(),
        EngineLimits::default(),
    )
    .await
    .expect("build community manager");

    ListenerComponents {
        auth: Arc::new(NoAuthProvider),
        auth_store,
        registry: Arc::new(manager),
        tmp,
    }
}

// ── Test doubles ────────────────────────────────────────────────────────────

/// A [`GraphAccessor`] whose `execute_query` always returns the engine
/// query-timeout sentinel error, exactly as
/// [`ermya_graph_server::DefaultGraphAccessor`] would after the engine aborts
/// a runaway query on an expired deadline. Every other method is an inert stub —
/// the boundary test only drives `execute_query`.
///
/// Does NOT read `Instant::now()` or `SystemTime::now()`: the abort is
/// deterministic and clock-free, so the test that uses it is not timing-flaky.
#[allow(dead_code)]
pub struct TimeoutAccessor;

impl GraphAccessor for TimeoutAccessor {
    fn execute_query(
        &self,
        _query: &ermya_graph::gql::GqlQuery,
        _params: std::collections::HashMap<String, ermya_graph::gql::GqlValue>,
        _max_rows: u64,
        _deadline: Option<std::time::Instant>,
    ) -> Result<Vec<ermya_graph_server::graph_accessor::ResultRow>, String> {
        // Mirror the string `engine_err_to_string` produces for a timeout abort:
        // the server-side sentinel prefix followed by the engine's message.
        Err(format!(
            "{}{}query exceeded time budget",
            ermya_graph_server::graph_accessor::ENGINE_QUERY_TIMEOUT_PREFIX,
            ermya_graph::gql::TIMEOUT_MSG_PREFIX,
        ))
    }

    fn execute_mutation(
        &self,
        _mutation: &ermya_graph::gql::MutationStatement,
        _params: std::collections::HashMap<String, ermya_graph::gql::GqlValue>,
        _deadline: Option<std::time::Instant>,
    ) -> Result<
        (
            Vec<ermya_graph_server::graph_accessor::ResultRow>,
            ermya_graph::gql::GqlMutationResult,
        ),
        String,
    > {
        Ok((Vec::new(), ermya_graph::gql::GqlMutationResult::default()))
    }

    fn execute_pipeline(
        &self,
        _pq: &ermya_graph::gql::PipelineQuery,
        _params: std::collections::HashMap<String, ermya_graph::gql::GqlValue>,
        _max_rows: u64,
        _deadline: Option<std::time::Instant>,
    ) -> Result<
        (
            Vec<ermya_graph_server::graph_accessor::ResultRow>,
            ermya_graph::gql::GqlMutationResult,
        ),
        String,
    > {
        Ok((Vec::new(), ermya_graph::gql::GqlMutationResult::default()))
    }

    fn execute_const_return(
        &self,
        _q: &ermya_graph::gql::ConstReturnQuery,
        _params: std::collections::HashMap<String, ermya_graph::gql::GqlValue>,
        _max_rows: u64,
        _deadline: Option<std::time::Instant>,
    ) -> Result<Vec<ermya_graph_server::graph_accessor::ResultRow>, String> {
        Ok(Vec::new())
    }

    fn execute_query_in_txn(
        &self,
        _txn_id: u64,
        _query: &ermya_graph::gql::GqlQuery,
        _params: std::collections::HashMap<String, ermya_graph::gql::GqlValue>,
        _max_rows: u64,
        _deadline: Option<std::time::Instant>,
    ) -> Result<Vec<ermya_graph_server::graph_accessor::ResultRow>, String> {
        // Same deterministic timeout sentinel as the auto-commit path, so a
        // transactional RUN exercises the same sentinel-to-wire-code mapping.
        Err(format!(
            "{}{}query exceeded time budget",
            ermya_graph_server::graph_accessor::ENGINE_QUERY_TIMEOUT_PREFIX,
            ermya_graph::gql::TIMEOUT_MSG_PREFIX,
        ))
    }

    fn execute_mutation_in_txn(
        &self,
        _txn_id: u64,
        _mutation: &ermya_graph::gql::MutationStatement,
        _params: std::collections::HashMap<String, ermya_graph::gql::GqlValue>,
        _deadline: Option<std::time::Instant>,
    ) -> Result<
        (
            Vec<ermya_graph_server::graph_accessor::ResultRow>,
            ermya_graph::gql::GqlMutationResult,
        ),
        String,
    > {
        Ok((Vec::new(), ermya_graph::gql::GqlMutationResult::default()))
    }

    fn execute_pipeline_in_txn(
        &self,
        _txn_id: u64,
        _pq: &ermya_graph::gql::PipelineQuery,
        _params: std::collections::HashMap<String, ermya_graph::gql::GqlValue>,
        _max_rows: u64,
        _deadline: Option<std::time::Instant>,
    ) -> Result<
        (
            Vec<ermya_graph_server::graph_accessor::ResultRow>,
            ermya_graph::gql::GqlMutationResult,
        ),
        String,
    > {
        Ok((Vec::new(), ermya_graph::gql::GqlMutationResult::default()))
    }

    fn execute_const_return_in_txn(
        &self,
        _txn_id: u64,
        _q: &ermya_graph::gql::ConstReturnQuery,
        _params: std::collections::HashMap<String, ermya_graph::gql::GqlValue>,
        _max_rows: u64,
        _deadline: Option<std::time::Instant>,
    ) -> Result<Vec<ermya_graph_server::graph_accessor::ResultRow>, String> {
        Ok(Vec::new())
    }

    fn begin_batch(&self) -> Result<(), String> {
        Ok(())
    }

    fn end_batch(&self) -> Result<(), String> {
        Ok(())
    }

    fn begin_txn(&self) -> Result<u64, String> {
        Ok(1)
    }

    fn commit_txn(&self, _txn_id: u64) -> Result<(), String> {
        Ok(())
    }

    fn rollback_txn(&self, _txn_id: u64) -> Result<(), String> {
        Ok(())
    }

    fn graph_arc(&self) -> Arc<std::sync::RwLock<Graph>> {
        Arc::new(std::sync::RwLock::new(Graph::new()))
    }
}

/// Spawn a [`BoltHandler`] backed by a real registry (holding
/// [`DEFAULT_TEST_DB`] granted to `anonymous`) whose per-session accessor is
/// always a [`TimeoutAccessor`] double — every `execute_query` returns the
/// engine timeout sentinel deterministically, without touching the system
/// clock. The handler-side deadline machinery is wired via `query_timeout_ms`
/// so the `query_timed_out` audit event echoes the configured value.
///
/// Used by `query_timeout_surfaces_execution_failed_and_audit_event` to assert
/// the handler maps the sentinel to `Neo.ClientError.Statement.ExecutionFailed`
/// and emits the audit event — the test the `G: GraphAccessor` generic used to
/// drive before Plan B removed it.
#[allow(dead_code)]
pub async fn spawn_bolt_handler_no_auth_with_timeout_double(
    query_timeout_ms: u64,
) -> HandlerWithAudit {
    use ermya_graph_server::auth::NoAuthProvider;
    use ermya_graph_server::registry::DbHandle;

    use ermya_graph_server::registry::{COMMUNITY_DATABASE, EngineLimits, SingleDatabaseManager};

    let tmp = tempfile::TempDir::new().expect("tempdir for timeout-double handler");
    let store = Arc::new(
        SystemGraphAuthStore::new(Arc::new(std::sync::RwLock::new(Graph::new())))
            .expect("system graph store"),
    );
    let auth_store: Arc<dyn UserStore> = store;
    let registry = Arc::new(
        SingleDatabaseManager::new(
            Arc::clone(&auth_store),
            tmp.path().join("databases").join(COMMUNITY_DATABASE),
            COMMUNITY_DATABASE.to_owned(),
            EngineLimits::default(),
        )
        .await
        .expect("build community manager"),
    ) as Arc<dyn GraphRegistry>;

    let audit_path = tmp.path().join("audit.log");
    let (audit_shutdown_tx, audit_shutdown_rx) = tokio::sync::watch::channel(false);
    let audit = AuditSink::file(audit_path.clone(), 1_000_000, 3, 0, audit_shutdown_rx)
        .expect("audit sink");

    let (client_stream, server_stream) = tokio::io::duplex(65_536);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let query_cache = Arc::new(ermya_graph_cypher::cache::QueryCache::new(256));

    // Every DB bind installs a `TimeoutAccessor`, so the first RUN exercises the
    // sentinel-to-wire-code mapping regardless of which tenant resolves.
    let factory: ermya_graph_server::AccessorFactory =
        Arc::new(|_h: &DbHandle| Arc::new(TimeoutAccessor) as Arc<dyn GraphAccessor>);

    tokio::spawn(async move {
        match BoltHandler::new_with_handshake(
            server_stream,
            Arc::new(NoAuthProvider),
            auth_store,
            audit,
            registry,
            // Montaje público: ni gestor de pago ni despachador de pago.
            None,
            None,
            query_cache,
            Duration::from_secs(30),
            0,
            0,
            0,
            0,
            0,
            query_timeout_ms,
            format!("Neo4j/{}", env!("CARGO_PKG_VERSION")),
            None,
            None,
            shutdown_rx,
        )
        .await
        {
            Ok(handler) => {
                let _ = handler.with_accessor_factory(factory).run().await;
            }
            Err(e) => {
                eprintln!("bolt handler error: {e}");
            }
        }
    });

    let (mut client_read, mut client_write) = tokio::io::split(client_stream);
    let mut handshake = [0u8; 20];
    handshake[..4].copy_from_slice(&BOLT_MAGIC);
    handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
    client_write.write_all(&handshake).await.unwrap();
    client_write.flush().await.unwrap();
    let mut resp = [0u8; 4];
    client_read.read_exact(&mut resp).await.unwrap();
    assert_eq!(
        resp,
        [0x00, 0x00, 0x04, 0x04],
        "bolt handshake version mismatch"
    );

    (
        BoltChunkedWriter::new(client_write),
        BoltChunkedReader::new(client_read),
        shutdown_tx,
        audit_shutdown_tx,
        tmp,
        audit_path,
    )
}

/// El gestor de **una sola base**, montado como andamio para las pruebas que no
/// afirman nada sobre catálogo ni permisos.
///
/// # Por qué existe
///
/// Apartado 5.14 del inventario. Las pruebas comunes del manejador de sesión
/// —consultas, transacciones, parámetros, paginación, topes— necesitan **un
/// manejador con una base detrás**, y nada más. Lo conseguían montando el gestor
/// multi-base, que además crea una base en el catálogo y reparte permisos: dos
/// cosas que la edición pública no tiene y que ninguna de esas pruebas
/// comprueba.
///
/// # El nombre de la base da igual, y es a propósito
///
/// Las pruebas piden [`DEFAULT_TEST_DB`], que no es la base de esta edición. No
/// importa: un gestor de una sola base **sirve la suya venga el nombre que
/// venga**, porque no usa el nombre como clave de búsqueda.
#[allow(dead_code)]
pub async fn single_db_registry() -> (Arc<dyn GraphRegistry>, tempfile::TempDir) {
    use ermya_graph_server::registry::{COMMUNITY_DATABASE, EngineLimits, SingleDatabaseManager};

    let tmp = tempfile::TempDir::new().expect("tempdir for single-db registry");
    let store = Arc::new(
        SystemGraphAuthStore::new(Arc::new(std::sync::RwLock::new(Graph::new())))
            .expect("system graph store"),
    );
    let manager = SingleDatabaseManager::new(
        Arc::clone(&store) as Arc<dyn UserStore>,
        tmp.path().join("databases").join(COMMUNITY_DATABASE),
        COMMUNITY_DATABASE.to_owned(),
        EngineLimits::default(),
    )
    .await
    .expect("build single-database manager");
    (Arc::new(manager) as Arc<dyn GraphRegistry>, tmp)
}

/// Monta un manejador sobre el gestor de una sola base, sin auditoría.
///
/// Equivalente público de [`spawn_bolt_handler`]: mismo tipo de retorno, misma
/// forma de uso, sin catálogo ni permisos detrás.
#[allow(dead_code)]
pub async fn spawn_single_db_handler<A>(
    auth: Arc<A>,
    registry: Arc<dyn GraphRegistry>,
) -> (
    BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::sync::watch::Sender<bool>,
)
where
    A: AuthProvider + 'static,
{
    spawn_bolt_handler_with_community_manager(
        auth,
        default_auth_store(),
        AuditSink::off(),
        registry,
    )
    .await
}

// ── Arranque del servidor público ───────────────────────────────────────────

/// Arranca un servidor de esta edición y espera a que esté escuchando.
///
/// Sustituye a las dos puertas de arranque cableadas que el árbol público ya no
/// ofrece. Aquellas construían el gestor por dentro y eran, por tanto, de una
/// edición concreta; la puerta que queda es neutra: recibe la factoría del
/// gestor y no sabe cuál le dan. Este ayudante le pasa la de una sola base, que
/// es la de esta edición, y ningún enganche de pago.
///
/// Devuelve la tarea del servidor —para poder esperar su resultado al apagar— y
/// la dirección donde quedó escuchando.
///
/// El nombre y la firma imitan a los de la puerta desaparecida a propósito: lo
/// que estas pruebas comprueban no cambia por el reparto, sólo cambia por dónde
/// se entra.
#[allow(dead_code)]
pub async fn start_community_server_with_ready(
    cfg: ermya_graph_server::ServerConfig,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> (
    tokio::task::JoinHandle<ermya_graph_server::Result<ermya_graph_server::ServerHandle>>,
    ermya_graph_server::ServerReady,
) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        ermya_graph_server::start_server_with_registry(
            cfg,
            shutdown_rx,
            Some(ready_tx),
            ermya_graph_server::single_database_factory(),
            ermya_graph_server::startup::PaidStartupHooks::default(),
        )
        .await
    });

    let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx)
        .await
        .expect("el canal de arranque no entregó la dirección en 10 s")
        .expect("el emisor de arranque se cerró sin enviar");

    (task, ready)
}

/// Arranca un servidor de esta edición sin esperar a que esté escuchando.
///
/// Para las pruebas que sólo miran el valor que devuelve el arranque —que
/// rechace un directorio de datos con un formato que no entiende, por ejemplo—.
/// Ahí el servidor nunca llega a escuchar, así que esperar la dirección
/// colgaría la prueba para siempre.
#[allow(dead_code)]
pub async fn start_community_server(
    cfg: ermya_graph_server::ServerConfig,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> ermya_graph_server::Result<ermya_graph_server::ServerHandle> {
    ermya_graph_server::start_server_with_registry(
        cfg,
        shutdown_rx,
        None,
        ermya_graph_server::single_database_factory(),
        ermya_graph_server::startup::PaidStartupHooks::default(),
    )
    .await
}

// ── Tope de intentos de autenticación ───────────────────────────────────────

/// Montaje compartido por varias conexiones que atacan la misma dirección de
/// origen, para poder ejercer el tope de intentos fallidos de autenticación.
///
/// El tope es **por dirección de origen y acumulado entre conexiones**, así que
/// no se puede comprobar con una sola: una conexión que falla el saludo queda
/// muerta. Hace falta abrir varias contra el mismo montaje, que es lo que este
/// tipo mantiene vivo — el limitador, el destino de auditoría y el directorio
/// temporal son compartidos; lo demás es por conexión.
///
/// Limitar los intentos de entrada no es una función de pago: protege al
/// servidor de un ataque por fuerza bruta y vale igual con una base que con
/// muchas. Por eso el montaje va sobre el gestor de una sola base.
#[allow(dead_code)]
pub struct AuthRateLimitFixture {
    auth: Arc<ermya_graph_server::auth::SystemGraphAuthProvider>,
    auth_store: Arc<dyn UserStore>,
    audit: AuditSink,
    audit_shutdown_tx: tokio::sync::watch::Sender<bool>,
    registry: Arc<dyn GraphRegistry>,
    rate_limiter: Arc<ermya_graph_server::rate_limiter::RateLimiter>,
    peer_ip: std::net::IpAddr,
    pub audit_path: std::path::PathBuf,
    pub tmp: tempfile::TempDir,
}

#[allow(dead_code)]
impl AuthRateLimitFixture {
    /// `auth_cap` es cuántos fallos por minuto se toleran desde una dirección
    /// antes de rechazar sin llegar a mirar las credenciales.
    pub async fn new(auth_cap: u32) -> Self {
        use std::net::{IpAddr, Ipv4Addr};
        use ermya_graph_server::auth::{SecretString, SystemGraphAuthProvider};
        use ermya_graph_server::rate_limiter::RateLimiter;
        use ermya_graph_server::registry::{
            COMMUNITY_DATABASE, EngineLimits, SingleDatabaseManager,
        };

        let tmp = tempfile::TempDir::new().expect("tempdir for auth-rate fixture");
        let store = Arc::new(
            SystemGraphAuthStore::new(Arc::new(std::sync::RwLock::new(Graph::new())))
                .expect("system graph store"),
        );
        // La cuenta que las pruebas intentan suplantar: tiene que existir para
        // que el fallo sea "credenciales incorrectas" y no "usuario
        // desconocido", que es otro camino.
        store
            .create_user("alice", &SecretString::new("passw0rd12".into()), false)
            .await
            .expect("create user");

        let auth = Arc::new(SystemGraphAuthProvider::from_store(Arc::clone(&store)));
        let auth_store: Arc<dyn UserStore> = store;

        let audit_path = tmp.path().join("audit.log");
        let (audit_shutdown_tx, audit_shutdown_rx) = tokio::sync::watch::channel(false);
        let audit = AuditSink::file(audit_path.clone(), 1_000_000, 3, 0, audit_shutdown_rx)
            .expect("audit sink");

        let registry = Arc::new(
            SingleDatabaseManager::new(
                Arc::clone(&auth_store),
                tmp.path().join("databases").join(COMMUNITY_DATABASE),
                COMMUNITY_DATABASE.to_owned(),
                EngineLimits::default(),
            )
            .await
            .expect("build community manager"),
        ) as Arc<dyn GraphRegistry>;

        let rate_limiter =
            RateLimiter::new(/* ip_cap */ 64, auth_cap, /* conn_per_ip */ 0);
        let peer_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 99));

        Self {
            auth,
            auth_store,
            audit,
            audit_shutdown_tx,
            registry,
            rate_limiter,
            peer_ip,
            audit_path,
            tmp,
        }
    }

    /// Abre una conexión más contra el mismo montaje, conservándolo vivo.
    pub async fn spawn_extra_handler(
        &self,
    ) -> (
        BoltChunkedWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
        BoltChunkedReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let (client_stream, server_stream) = tokio::io::duplex(65_536);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let query_cache = Arc::new(ermya_graph_cypher::cache::QueryCache::new(256));

        let auth = Arc::clone(&self.auth);
        let auth_store = Arc::clone(&self.auth_store);
        let audit = self.audit.clone();
        let registry = Arc::clone(&self.registry);
        let rate_limiter = Arc::clone(&self.rate_limiter);
        let peer_ip = self.peer_ip;

        tokio::spawn(async move {
            match BoltHandler::new_with_handshake(
                server_stream,
                auth,
                auth_store,
                audit,
                registry,
                // Montaje público: ni gestor de pago ni despachador de pago.
                None,
                None,
                query_cache,
                Duration::from_secs(30),
                0,
                0,
                0,
                0,
                0,
                0,
                format!("Neo4j/{}", env!("CARGO_PKG_VERSION")),
                Some(rate_limiter),
                Some(peer_ip),
                shutdown_rx,
            )
            .await
            {
                Ok(mut handler) => {
                    let _ = handler.run().await;
                }
                Err(e) => eprintln!("bolt handler error: {e}"),
            }
        });

        let (mut client_read, mut client_write) = tokio::io::split(client_stream);
        let mut handshake = [0u8; 20];
        handshake[..4].copy_from_slice(&BOLT_MAGIC);
        handshake[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
        client_write.write_all(&handshake).await.unwrap();
        client_write.flush().await.unwrap();
        let mut resp = [0u8; 4];
        client_read.read_exact(&mut resp).await.unwrap();
        assert_eq!(
            resp,
            [0x00, 0x00, 0x04, 0x04],
            "bolt handshake version mismatch"
        );

        (
            BoltChunkedWriter::new(client_write),
            BoltChunkedReader::new(client_read),
            shutdown_tx,
        )
    }

    /// Vuelca los sucesos de auditoría acumulados hasta ahora.
    pub async fn drain_audit(&self) -> Vec<serde_json::Value> {
        read_audit_events(&self.audit_shutdown_tx, &self.audit_path).await
    }
}
