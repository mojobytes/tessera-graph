// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! Integration tests for [`start_server`].

#[cfg(feature = "plain-tcp")]
#[allow(dead_code)]
#[path = "common/mod.rs"]
mod common;

#[cfg(feature = "plain-tcp")]
mod tests {
    #[allow(unused_imports)]
    use super::common;
    use std::time::Duration;

    // Cycle 11.7 promoted the Bolt-client imports out of the
    // `#[cfg(any())]`-gated block below — they are now consumed by
    // `registry_acquire_visible_after_hello`. The two disabled
    // password-auth tests at the bottom of this module reuse the same
    // re-exports so they compile cleanly when Task 9 reintroduces them.

    use tessera_graph_server::config::ServerConfig;
    use tessera_graph_server::migration::CURRENT_DISK_LAYOUT;
    #[cfg(unix)]
    use tessera_graph_server::migration::write;

    // Cycle 11.7 imports a real Bolt client over TCP; these symbols are
    // also referenced by the `#[cfg(any())]`-gated password tests below,
    // so promote the imports to the parent module instead of duplicating.
    use tessera_graph_protocol::bolt_frame::{BoltChunkedReader, BoltChunkedWriter};
    use tessera_graph_protocol::bolt_message::{BoltRequest, BoltResponse};
    use tessera_graph_protocol::{BOLT_MAGIC, PackStreamValue, decode_response, encode_request};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Open a Bolt connection to `addr`, drive the handshake, send a HELLO
    // for `admin`/`admin-pw-12chars` (auth-only post-Task-10-bis), then
    // send a RUN with `extra["db"] = db_name` and return the RUN reply.
    //
    // The RUN reply is the right probe because the registry only
    // exercises `try_bind_database` on the first RUN that carries
    // `extra["db"]` — HELLO no longer opens any database.
    async fn hello_then_run_admin_to(
        addr: std::net::SocketAddr,
        connect_deadline: std::time::Instant,
        db_name: &str,
    ) -> BoltResponse {
        let stream = loop {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < connect_deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(e) => panic!("server did not bind {addr} within deadline: {e}"),
            }
        };
        let (mut read, mut write_half) = tokio::io::split(stream);
        let mut hs = [0u8; 20];
        hs[..4].copy_from_slice(&BOLT_MAGIC);
        hs[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
        write_half.write_all(&hs).await.unwrap();
        write_half.flush().await.unwrap();
        let mut ver = [0u8; 4];
        read.read_exact(&mut ver).await.unwrap();
        assert_eq!(ver, [0x00, 0x00, 0x04, 0x04], "bolt version mismatch");
        let mut cw = BoltChunkedWriter::new(write_half);
        let mut cr = BoltChunkedReader::new(read);

        let hello = BoltRequest::Hello {
            extra: vec![
                (
                    "principal".to_owned(),
                    PackStreamValue::String("admin".to_owned()),
                ),
                (
                    "credentials".to_owned(),
                    PackStreamValue::String("admin-pw-12chars".to_owned()),
                ),
            ],
        };
        cw.write_message(&encode_request(&hello).unwrap())
            .await
            .unwrap();
        let hello_resp = cr.read_message().await.unwrap().expect("HELLO reply");
        let hello_resp = decode_response(&hello_resp).unwrap();
        assert!(
            matches!(hello_resp, BoltResponse::Success { .. }),
            "expected SUCCESS for HELLO admin, got {hello_resp:?}"
        );

        let run = BoltRequest::Run {
            query: "RETURN 1".to_owned(),
            params: vec![],
            extra: vec![(
                "db".to_owned(),
                PackStreamValue::String(db_name.to_owned()),
            )],
        };
        cw.write_message(&encode_request(&run).unwrap())
            .await
            .unwrap();
        let run_resp = cr.read_message().await.unwrap().expect("RUN reply");
        decode_response(&run_resp).unwrap()
    }

    // Open a Bolt connection to `addr`, drive the handshake, and send a HELLO
    // carrying NO `principal`/`credentials`. With `no_auth = true` the server
    // must accept the anonymous HELLO with SUCCESS; with auth on it would
    // FAIL. Reuses the same handshake as `hello_then_run_admin_to` rather than
    // duplicating it.
    async fn hello_no_credentials_to(
        addr: std::net::SocketAddr,
        connect_deadline: std::time::Instant,
    ) -> BoltResponse {
        let stream = loop {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < connect_deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(e) => panic!("server did not bind {addr} within deadline: {e}"),
            }
        };
        let (mut read, mut write_half) = tokio::io::split(stream);
        let mut hs = [0u8; 20];
        hs[..4].copy_from_slice(&BOLT_MAGIC);
        hs[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
        write_half.write_all(&hs).await.unwrap();
        write_half.flush().await.unwrap();
        let mut ver = [0u8; 4];
        read.read_exact(&mut ver).await.unwrap();
        assert_eq!(ver, [0x00, 0x00, 0x04, 0x04], "bolt version mismatch");
        let mut cw = BoltChunkedWriter::new(write_half);
        let mut cr = BoltChunkedReader::new(read);

        // No principal, no credentials: the anonymous HELLO that `no_auth`
        // is supposed to wave through.
        let hello = BoltRequest::Hello { extra: vec![] };
        cw.write_message(&encode_request(&hello).unwrap())
            .await
            .unwrap();
        let hello_resp = cr.read_message().await.unwrap().expect("HELLO reply");
        decode_response(&hello_resp).unwrap()
    }

    // ── Cycle 6.1: start_server binds and shuts down ────────────────────────

    #[tokio::test]
    async fn start_server_binds_and_shuts_down() {
        // Isolate `data_dir`: since v0.7.0 the default is the production path
        // `/var/lib/tessera/data` (root-only on a dev host). Without this the
        // spawned task returns `Err(PermissionDenied)` immediately, the join
        // never times out, and the old `result.is_ok()` assertion passed
        // without the server ever having bound — a false green.
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            tessera_graph_server::start_server_with_registry(
                config,
                shutdown_rx,
                None,
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = shutdown_tx.send(true);

        let joined = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server did not shut down within 5 seconds")
            .expect("join");
        // Assert on the actual `start_server` result, not only the join: the
        // server must have bound and shut down cleanly, not errored out.
        assert!(
            joined.is_ok(),
            "start_server must return Ok after a clean bind + shutdown, got {:?}",
            joined.as_ref().err()
        );
    }

    // ── Cycle 6.2: start_server with password rejects bad auth ──────────────

    // ── Cycle 11.6: start_server returns ServerHandle with registry ─────────


    // ── Cycle 11.4: schema-version guard rejects too-old layout ─────────────

    #[tokio::test]
    async fn startup_rejects_outdated_layout() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-write a marker pinning a pre-multidb layout. The v0.4 →
        // v0.5 migration is what bumps `1 → 2`; the binary must refuse
        // to attach to a directory still on `1` and surface the CLI
        // call to action.
        std::fs::write(
            dir.path().join(".tessera-version"),
            r#"{"disk_layout":1,"last_migrated_at_ms":1714051200000}"#,
        )
        .unwrap();

        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        let (_tx, rx) = tokio::sync::watch::channel(false);
        let err = tokio::time::timeout(
            Duration::from_secs(3),
            tessera_graph_server::start_server_with_registry(
                cfg,
                rx,
                None,
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            ),
        )
        .await
        .expect("start_server must return without waiting for shutdown")
        .expect_err(
            "start_server must refuse a data_dir pinned to an older layout",
        );
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("out of date"),
            "error did not flag the layout as out of date: {err}"
        );
        assert!(
            msg.contains("migrate"),
            "error did not point at the CLI migrator: {err}"
        );
        assert!(
            msg.contains("found layout 1"),
            "error did not echo the on-disk layout: {err}"
        );
    }

    // ── Cycle 11.4: schema-version guard rejects too-new layout ─────────────

    #[tokio::test]
    async fn startup_rejects_too_new_layout() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-write a marker from a hypothetical future server. The
        // binary must refuse rather than risk re-writing pages it
        // does not understand.
        std::fs::write(
            dir.path().join(".tessera-version"),
            r#"{"disk_layout":99,"last_migrated_at_ms":4102444800000}"#,
        )
        .unwrap();

        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        let (_tx, rx) = tokio::sync::watch::channel(false);
        let err = tokio::time::timeout(
            Duration::from_secs(3),
            tessera_graph_server::start_server_with_registry(
                cfg,
                rx,
                None,
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            ),
        )
        .await
        .expect("start_server must return without waiting for shutdown")
        .expect_err(
            "start_server must refuse a data_dir pinned to a newer layout",
        );
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("newer than server"),
            "error did not flag the layout as too new: {err}"
        );
        assert!(
            msg.contains("upgrade server"),
            "error did not suggest upgrading the server: {err}"
        );
        assert!(
            msg.contains("found layout 99"),
            "error did not echo the on-disk layout: {err}"
        );
    }

    // ── Cycle 11.5: fresh-install bootstrap stamps the version marker ───────

    #[tokio::test]
    async fn startup_stamps_version_on_fresh_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Genuinely empty data_dir: no `system/`, no `.tessera-version`.
        // Startup must treat this as a fresh install, stamp the
        // current layout, and proceed — distinguishable from a v0.4
        // dir, which has `system/` but no marker and is rejected.
        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            tessera_graph_server::start_server_with_registry(
                cfg,
                rx,
                None,
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = tx.send(true);
        let _handle = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("start_server did not shut down")
            .expect("join")
            .expect("start_server must accept a fresh data_dir");

        // The marker is stamped at startup with the current layout.
        let marker = dir.path().join(".tessera-version");
        assert!(
            marker.exists(),
            "fresh-install startup must stamp `.tessera-version`"
        );
        let body = std::fs::read_to_string(&marker).unwrap();
        assert!(
            body.contains(&format!("\"disk_layout\": {CURRENT_DISK_LAYOUT}")),
            "marker did not pin the current layout: {body}"
        );
    }

    // ── Cycle 11.5: v0.4 → v0.5 upgrade case still rejected ─────────────────

    #[tokio::test]
    async fn startup_rejects_populated_dir_without_marker() {
        let dir = tempfile::tempdir().unwrap();
        // A populated v0.4 data_dir has `system/` from a previous run
        // but no `.tessera-version`. The fresh-install heuristic must
        // refuse it so the operator runs the CLI migrator instead of
        // silently inheriting v0.4 pages under the v0.5 binary.
        std::fs::create_dir_all(dir.path().join("system")).unwrap();

        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        let (_tx, rx) = tokio::sync::watch::channel(false);
        let err = tokio::time::timeout(
            Duration::from_secs(3),
            tessera_graph_server::start_server_with_registry(
                cfg,
                rx,
                None,
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            ),
        )
        .await
        .expect("start_server must return without waiting for shutdown")
        .expect_err(
            "start_server must refuse a populated v0.4 data_dir without marker",
        );
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("disk layout") && msg.contains("migrate"),
            "error did not surface the migration call to action: {err}"
        );
    }

    // ── Cycle 9.1: bootstrap admin on empty system graph ────────────────────

    #[tokio::test]
    async fn startup_bootstraps_admin_on_empty_system_graph() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            tessera_graph_server::start_server_with_registry(
                cfg,
                rx,
                None,
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = tx.send(true);
        let server_handle = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("first start_server did not shut down")
            .expect("join")
            .expect("start_server returned Err");
        assert_ne!(server_handle.addr.port(), 0);

        // Re-open: admin already exists, bootstrap must be idempotent.
        let cfg2 = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let (tx2, rx2) = tokio::sync::watch::channel(false);
        let h2 = tokio::spawn(async move {
            tessera_graph_server::start_server_with_registry(
                cfg2,
                rx2,
                None,
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = tx2.send(true);
        let _handle2 = tokio::time::timeout(Duration::from_secs(5), h2)
            .await
            .expect("second start_server did not shut down")
            .expect("join")
            .expect("second start_server returned Err");
    }

    // ── Cycle 9.2: permission refusal on Unix ───────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_refuses_world_readable_system_dir() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        // Cycle 11.3: pre-write the schema marker so the guard passes
        // and `enforce_system_dir_perms` is the next gate to fire — the
        // contract this test still exercises.
        write(dir.path(), CURRENT_DISK_LAYOUT).unwrap();
        let system = dir.path().join("system");
        std::fs::create_dir_all(&system).unwrap();
        std::fs::set_permissions(
            &system,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        let (_tx, rx) = tokio::sync::watch::channel(false);
        let err = tessera_graph_server::start_server_with_registry(
            cfg,
            rx,
            None,
            tessera_graph_server::single_database_factory(),
            tessera_graph_server::startup::PaidStartupHooks::default(),
        )
            .await
            .expect_err("start_server should refuse world-readable system dir");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("permission"),
            "error did not mention permissions: {err}"
        );
    }

    // ── Cycle 9.3: no_auth bypass ───────────────────────────────────────────

    #[tokio::test]
    async fn startup_respects_no_auth_env_var() {
        // v0.7.0 made `data_dir` default to the production path
        // `/var/lib/tessera/data`, which is root-only on a dev host. Isolate
        // to a tempdir so this test exercises the `no_auth` path rather than
        // failing in the fail-safe data-dir check — every other startup test
        // already does this.
        let dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            no_auth: true,
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = tokio::spawn(async move {
            tessera_graph_server::start_server_with_registry(
                cfg,
                shutdown_rx,
                Some(ready_tx),
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            )
            .await
        });

        let addr = tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("ready channel did not deliver addr within 5 s")
            .expect("ready sender dropped before sending addr")
            .bolt_addr;

        // The actual contract of `no_auth`: an anonymous HELLO (no principal,
        // no credentials) is accepted. With auth on this would FAIL, so the
        // SUCCESS here is what proves the bypass took effect — not merely that
        // the server bound a port.
        let connect_deadline = std::time::Instant::now() + Duration::from_secs(1);
        let resp = hello_no_credentials_to(addr, connect_deadline).await;
        assert!(
            matches!(resp, BoltResponse::Success { .. }),
            "no_auth=true must accept an anonymous HELLO with SUCCESS, got {resp:?}"
        );

        // Clean shutdown still has to succeed.
        let _ = shutdown_tx.send(true);
        let _handle = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("start_server did not shut down")
            .expect("join")
            .expect("start_server returned Err");
    }

    // ── Cycle 11.7 (+ v0.5.0 Task 10-bis cycle 7 update): ──────────────────
    //
    // Registry is wired into the live accept loop. The discriminator has
    // moved from HELLO (pre-Task-10-bis) to the first RUN (post): HELLO
    // only authenticates, and `try_bind_database` runs on the first RUN
    // that carries `extra["db"]`. A RUN naming a database absent from
    // the catalog still returns `Database.DatabaseNotFound` AND bumps
    // `open_failures_total` on the `Arc<DatabaseRegistry>` exposed
    // through `ServerHandle` — that branch is reachable ONLY through
    // the registry wiring, so it remains the right probe.

    // ── Cycle 11.8: shutdown drains the registry via close_all ──────────────
    //
    // start_server must invoke `registry.close_all(shutdown_timeout)` between
    // the accept loop returning and `drop(system_lock)` so live database
    // entries are evicted in an orderly fashion. The test pre-populates the
    // system graph with admin + a `tenanta` database, drives a HELLO that
    // opens it through the wired registry, then triggers shutdown. Without
    // the wiring `closes_total` stays at zero (the map is dropped along with
    // the registry but no eviction is recorded). With the wiring it reflects
    // the drained entry. Timing is bounded by `shutdown_timeout_seconds`
    // plus a generous join margin to keep the test stable on busy CI hosts.

    // ── Cycle 11.9: max_open_databases propagates from ServerConfig ─────────
    //
    // `ServerConfig.max_open_databases` must reach `RegistryConfig`. The
    // discriminator: pre-create two databases, set `cap = 1`, and open the
    // first one (success). A second connection asking for the second
    // database must hit `OpenCapExceeded` because the first slot is
    // occupied. Pre-cycle 9 the cap was hardcoded to `None`, so both
    // HELLOs would succeed.
    //
    // The test also pins the QR-fix (finding 11): the wire `message` for
    // the second HELLO must NOT contain the configured cap value or the
    // `max_open_databases` literal — `registry_error_to_wire_message`
    // flattens it to the generic transient string.
    //
    // `cap = 0` would have been the simplest discriminator pre-QR, but
    // `DatabaseRegistry::new` now coerces `Some(0)` → `None` (every
    // acquire would be sanitised) so the test would no longer pin the
    // wiring.

    // ── Cycle QR-1: start_server reports the bound address via oneshot ──────
    //
    // Task 11 QR finding 6: the original three integration tests reserved an
    // ephemeral port via probe-bind-drop because `start_server` only exposed
    // `addr` post-shutdown. The new contract is the ready channel:
    // the caller passes a `oneshot::Sender<SocketAddr>` and receives the
    // bound address as soon as the listener is up — well before any client
    // connection. The probe-bind-drop pattern is then deletable.
    //
    // This test is the RED end of the cycle: the symbol must compile, accept
    // `bind_addr: "127.0.0.1:0"`, and deliver the resolved port through the
    // oneshot within a small budget (the bind happens before any await
    // serving, so 1 s is generous). The receiver also proves the server is
    // truly bound — TcpStream::connect after the oneshot must succeed
    // without the racy retry loop the legacy tests carry.
    #[tokio::test]
    async fn start_server_reports_bound_addr_via_oneshot() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let server_task = tokio::spawn(async move {
            tessera_graph_server::start_server_with_registry(
                cfg,
                shutdown_rx,
                Some(ready_tx),
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            )
            .await
        });

        // The bound address must arrive promptly — start_server resolves it
        // right after `TesseraListener::bind` and before entering the accept
        // loop. 1 s covers cold-start of the system graph + audit sink + auth
        // bootstrap on a slow CI host.
        let addr = tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("ready channel did not deliver addr within 1 s")
            .expect("ready sender dropped before sending addr")
            .bolt_addr;

        assert_ne!(addr.port(), 0, "ephemeral port must be resolved");
        assert!(addr.ip().is_loopback(), "bind addr must stay on loopback");

        // Connecting must succeed on the first try — no retry loop. If the
        // oneshot fired but the listener is not really up, this catches it.
        let _stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("server must accept connections immediately after ready");

        let _ = shutdown_tx.send(true);
        let server_handle = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server did not shut down within 5 s")
            .expect("join")
            .expect("el arranque devolvió Err");
        assert_eq!(
            server_handle.addr, addr,
            "ServerHandle.addr must match the address delivered through the oneshot"
        );
    }

    // Disabled by Task 4; Task 9 reintroduces via SystemGraphAuthProvider.
    #[cfg(any())]
    #[tokio::test]
    async fn start_server_with_password_rejects_bad_credentials() {
        let config = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            password: Some("correct-pw".to_owned()),
            ..Default::default()
        };

        // We need the bound address. start_server returns it, but it blocks.
        // So we use a channel to communicate the address.
        let (addr_tx, addr_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            // Bind manually so we can extract the address before serving.
            let listener =
                tessera_graph_server::TesseraListener::bind(&config.bind_addr)
                    .await
                    .unwrap();
            let addr = listener.local_addr().unwrap();
            let _ = addr_tx.send(addr);

            let auth: std::sync::Arc<dyn tessera_graph_server::auth::AuthProvider> =
                std::sync::Arc::new(
                    tessera_graph_server::auth::PasswordAuthProvider::new("correct-pw"),
                );
            let graph = std::sync::Arc::new(
                tessera_graph_server::DefaultGraphAccessor::new(std::sync::Arc::new(
                    std::sync::RwLock::new(tessera_graph::Graph::new()),
                )),
            );

            let _ = listener
                .serve_plain(
                    auth,
                    common::default_auth_store(),
                    tessera_graph_server::audit::AuditSink::off(),
                    graph,
                    shutdown_rx,
                    10,
                    Duration::from_secs(30),
                )
                .await;
        });

        let addr = addr_rx.await.unwrap();

        // Connect as a Bolt client.
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut read, mut write) = tokio::io::split(stream);

        // Bolt handshake.
        let mut hs = [0u8; 20];
        hs[..4].copy_from_slice(&BOLT_MAGIC);
        hs[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
        write.write_all(&hs).await.unwrap();
        write.flush().await.unwrap();
        let mut ver = [0u8; 4];
        read.read_exact(&mut ver).await.unwrap();

        let mut cw = BoltChunkedWriter::new(write);
        let mut cr = BoltChunkedReader::new(read);

        // HELLO with wrong password → FAILURE.
        let hello = BoltRequest::Hello {
            extra: vec![
                (
                    "principal".to_owned(),
                    tessera_graph_protocol::PackStreamValue::String("admin".to_owned()),
                ),
                (
                    "credentials".to_owned(),
                    tessera_graph_protocol::PackStreamValue::String("wrong-pw".to_owned()),
                ),
            ],
        };
        let data = encode_request(&hello).unwrap();
        cw.write_message(&data).await.unwrap();

        let resp_data = cr.read_message().await.unwrap().expect("expected message");
        let resp = decode_response(&resp_data).unwrap();
        assert!(
            matches!(resp, BoltResponse::Failure { .. }),
            "expected FAILURE for wrong password, got {resp:?}"
        );

        let _ = shutdown_tx.send(true);
    }

    // Disabled by Task 4; Task 9 reintroduces via SystemGraphAuthProvider.
    #[cfg(any())]
    #[tokio::test]
    async fn start_server_with_password_accepts_correct_credentials() {
        let (addr_tx, addr_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            let listener =
                tessera_graph_server::TesseraListener::bind("127.0.0.1:0")
                    .await
                    .unwrap();
            let addr = listener.local_addr().unwrap();
            let _ = addr_tx.send(addr);

            let auth: std::sync::Arc<dyn tessera_graph_server::auth::AuthProvider> =
                std::sync::Arc::new(
                    tessera_graph_server::auth::PasswordAuthProvider::new("correct-pw"),
                );
            let graph = std::sync::Arc::new(
                tessera_graph_server::DefaultGraphAccessor::new(std::sync::Arc::new(
                    std::sync::RwLock::new(tessera_graph::Graph::new()),
                )),
            );

            let _ = listener
                .serve_plain(auth, common::default_auth_store(), tessera_graph_server::audit::AuditSink::off(), graph, shutdown_rx, 10, Duration::from_secs(30))
                .await;
        });

        let addr = addr_rx.await.unwrap();

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut read, mut write) = tokio::io::split(stream);

        let mut hs = [0u8; 20];
        hs[..4].copy_from_slice(&BOLT_MAGIC);
        hs[4..8].copy_from_slice(&0x0004_0404_u32.to_be_bytes());
        write.write_all(&hs).await.unwrap();
        write.flush().await.unwrap();
        let mut ver = [0u8; 4];
        read.read_exact(&mut ver).await.unwrap();

        let mut cw = BoltChunkedWriter::new(write);
        let mut cr = BoltChunkedReader::new(read);

        // HELLO with correct password → SUCCESS.
        let hello = BoltRequest::Hello {
            extra: vec![
                (
                    "principal".to_owned(),
                    tessera_graph_protocol::PackStreamValue::String("admin".to_owned()),
                ),
                (
                    "credentials".to_owned(),
                    tessera_graph_protocol::PackStreamValue::String("correct-pw".to_owned()),
                ),
            ],
        };
        let data = encode_request(&hello).unwrap();
        cw.write_message(&data).await.unwrap();

        let resp_data = cr.read_message().await.unwrap().expect("expected message");
        let resp = decode_response(&resp_data).unwrap();
        assert!(
            matches!(resp, BoltResponse::Success { .. }),
            "expected SUCCESS for correct password, got {resp:?}"
        );

        let _ = shutdown_tx.send(true);
    }

    // ── v0.7.0 Fase 3 Feature B (Cycle 5): actionable error on unwritable
    //    data_dir ────────────────────────────────────────────────────────────
    //
    // With the new file-backed default, a deployment that previously booted
    // clean (no TESSERA_DATA_DIR) now tries to write the data dir. If that path
    // is not creatable/writable, startup must fail with a message that names
    // the path AND points the operator at both escape hatches
    // (TESSERA_DATA_DIR and the :memory: sentinel) — never a cryptic OS error.
    #[cfg(unix)]
    #[tokio::test]
    async fn start_server_fails_with_actionable_error_on_unwritable_data_dir() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        // Make the parent unwritable so create_dir_all on a child fails.
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let data_dir = parent.path().join("locked/tdata");

        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(data_dir.clone()),
            ..Default::default()
        };

        let (_tx, rx) = tokio::sync::watch::channel(false);
        let result = tessera_graph_server::start_server_with_registry(
            cfg,
            rx,
            None,
            tessera_graph_server::single_database_factory(),
            tessera_graph_server::startup::PaidStartupHooks::default(),
        )
        .await;

        // Restore permissions so the tempdir can be cleaned up.
        let _ =
            std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o755));

        let err = result.expect_err("startup must fail when data_dir is unwritable");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains(&data_dir.display().to_string().to_lowercase()),
            "error must name the failing path; got: {msg}"
        );
        assert!(
            msg.contains("tessera_data_dir"),
            "error must mention TESSERA_DATA_DIR; got: {msg}"
        );
        assert!(
            msg.contains(":memory:"),
            "error must mention the :memory: opt-out; got: {msg}"
        );
    }

    // ── Split ciclo 6 (cierre): la factoría del gestor llega al arranque ────
    //
    // El seam `GraphRegistry` llegaba al handler pero NO al arranque: éste
    // construía `DatabaseRegistry` directamente y lo exponía con su tipo
    // concreto. Consecuencia: un servidor Community NO era arrancable por la
    // vía normal — el gestor de una sola base existía y estaba probado, pero
    // nadie podía encenderlo.
    //
    // Estos tests fijan el contrato de la factoría: quien arranca el servidor
    // decide qué gestor se monta, y el arranque no conoce ningún tipo
    // concreto. Es el mismo patrón que `AccessorFactory` ya usa para el
    // acceso al grafo.

    /// Arrancar con la factoría del gestor Community da un servidor que sirve.
    ///
    /// Esto es lo que hoy es IMPOSIBLE. El probe es una consulta real que
    /// escribe y lee de vuelta: un servidor que enlaza el puerto pero no
    /// sirve su única base pasaría un test de "arrancó" y fallaría éste.
    #[tokio::test]
    async fn community_manager_factory_yields_a_serving_server() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            password: Some("admin-pw-12chars".to_owned()),
            ..Default::default()
        };

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = tokio::spawn(async move {
            tessera_graph_server::start_server_with_registry(
                cfg,
                shutdown_rx,
                Some(ready_tx),
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            )
            .await
        });

        let addr = tokio::time::timeout(Duration::from_secs(10), ready_rx)
            .await
            .expect("el canal ready no entregó la dirección en 10 s")
            .expect("el emisor de ready se cerró sin enviar")
            .bolt_addr;

        // El servidor Community sirve su única base. `acquire` ignora el
        // nombre, así que cualquier nombre alcanza la misma base — lo que se
        // verifica es que la consulta SE EJECUTA, no que el puerto responde.
        let connect_deadline = std::time::Instant::now() + Duration::from_secs(2);
        let resp = hello_then_run_admin_to(addr, connect_deadline, "graph").await;
        assert!(
            matches!(resp, BoltResponse::Success { .. }),
            "el servidor Community debe servir su única base; llegó {resp:?}"
        );

        let _ = shutdown_tx.send(true);
        let handle = tokio::time::timeout(Duration::from_secs(10), server_task)
            .await
            .expect("el servidor no se apagó")
            .expect("join")
            .expect("start_server_with_registry devolvió Err");

        // Esta edición no tiene gestor multi-base, y el asa ya ni siquiera
        // ofrece por dónde preguntarlo: el tipo que lo rellenaría no existe
        // aquí. Lo que queda observable es que el servidor sirvió su única base
        // —comprobado arriba— y que el asa entrega un gestor, el suyo.
        assert!(
            std::sync::Arc::strong_count(&handle.registry) >= 1,
            "el asa debe entregar el gestor con el que sirvió"
        );
    }

    /// La factoría Community lanza la tarea que recupera memoria de versiones.
    ///
    /// Las transacciones explícitas son Community —el motor entero va en esa
    /// edición—, y cada una que confirma deja versiones en memoria hasta que
    /// algo las materializa. Esa tarea vivía sólo en el gestor multi-base, así
    /// que un servidor Community las acumulaba durante toda la vida del
    /// proceso: fuga de memoria en la edición pública, no reparto de
    /// funcionalidad.
    ///
    /// El test escribe en transacción a través del asa que la interfaz común
    /// entrega, y espera a que la tarea de fondo actúe sola. Comprobar que la
    /// operación existe no bastaría: existía en el gestor y nadie la llamaba.
    /// Lo que se vigila aquí es que **el arranque la ponga a correr**, que era
    /// justamente lo que faltaba.
    #[tokio::test]
    async fn community_startup_runs_the_version_memory_reclaim() {
        use tessera_graph::Properties;

        let dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".to_owned(),
            data_dir: Some(dir.path().to_path_buf()),
            password: Some("admin-pw-12chars".to_owned()),
            // Un segundo es el mínimo que el servidor acepta; con el valor por
            // defecto (300 s) el test tardaría cinco minutos.
            vacuum_interval_seconds: 1,
            ..Default::default()
        };

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = tokio::spawn(async move {
            tessera_graph_server::start_server_with_registry(
                cfg,
                shutdown_rx,
                Some(ready_tx),
                tessera_graph_server::single_database_factory(),
                tessera_graph_server::startup::PaidStartupHooks::default(),
            )
            .await
        });

        let _addr = tokio::time::timeout(Duration::from_secs(10), ready_rx)
            .await
            .expect("el canal ready no entregó la dirección en 10 s")
            .expect("el emisor de ready se cerró sin enviar")
            .bolt_addr;

        let _ = shutdown_tx.send(true);
        let handle = tokio::time::timeout(Duration::from_secs(10), server_task)
            .await
            .expect("el servidor no se apagó")
            .expect("join")
            .expect("start_server_with_registry devolvió Err");

        // Escribir y confirmar en transacción deja versiones vivas en memoria.
        let db = handle
            .registry
            .acquire("neo4j", "admin")
            .await
            .expect("acquire");
        {
            let graph = db.graph();
            let mut g = graph.write().expect("write lock");
            let txn = g.begin_txn().expect("begin");
            g.add_node_in_txn(txn, "N", Properties::new()).expect("write");
            g.commit_txn(txn).expect("commit");
        }
        drop(db);

        // Nadie llama a la recuperación desde el test: si el arranque no dejó
        // la tarea corriendo, esto no baja nunca y el test agota su espera.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let db = handle
                .registry
                .acquire("neo4j", "admin")
                .await
                .expect("acquire");
            let pending = {
                let graph = db.graph();
                let g = graph.read().expect("read lock");
                g.pending_version_chains()
            };
            drop(db);
            if pending == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "la tarea de fondo no recuperó la memoria de versiones: \
                 quedan {pending} cadenas tras 15 s con intervalo de 1 s"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

}
