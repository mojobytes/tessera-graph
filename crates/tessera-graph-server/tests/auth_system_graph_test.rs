// SPDX-License-Identifier: BSL-1.1

//! Pruebas del almacén de identidad sobre el grafo de sistema — **la parte que
//! esta edición sí ofrece**: cuentas locales, contraseñas y autenticación.
//!
//! El fichero está partido por temas (apartado 5.15 del inventario). Lo que
//! vive aquí sólo necesita la gestión de usuarios: crear, listar, borrar,
//! cambiar contraseña, activar y desactivar, y el privilegio de administrador,
//! más la protección de no quedarse sin ningún administrador activo.
//!
//! Lo que **no** está aquí —catálogo de bases y permisos: crear y borrar base,
//! conceder, revocar, listar permisos y calcular el acceso efectivo— vive en
//! `auth_system_graph_enterprise_test.rs` del árbol de pago, porque el almacén
//! sólo implementa esas operaciones donde existe el fichero que las aporta.

use std::sync::{Arc, RwLock};

use tessera_graph::Graph;
use tessera_graph_server::auth::{
    AuthError, AuthProvider, AuthStoreError, SecretString, SystemGraphAuthProvider,
    SystemGraphAuthStore, UserStore,
};

fn fresh_system_graph() -> Arc<RwLock<Graph>> {
    Arc::new(RwLock::new(Graph::new()))
}

fn fresh_store() -> Arc<SystemGraphAuthStore> {
    let graph = fresh_system_graph();
    Arc::new(SystemGraphAuthStore::new(graph).expect("store new"))
}

fn fresh_pair() -> (Arc<SystemGraphAuthProvider>, Arc<SystemGraphAuthStore>) {
    let store = fresh_store();
    let provider = Arc::new(SystemGraphAuthProvider::from_store(Arc::clone(&store)));
    (provider, store)
}

fn secret(p: &str) -> SecretString {
    SecretString::new(p.to_owned())
}

#[tokio::test]
async fn create_user_then_list_returns_it() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("alice", &pw, false).await.unwrap();

    let users = store.list_users().await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].username, "alice");
    assert!(!users[0].is_admin);
    assert!(users[0].enabled);
    assert!(!users[0].created_at.is_empty());
}

#[tokio::test]
async fn create_user_normalises_username_lowercase() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("Alice", &pw, false).await.unwrap();

    let users = store.list_users().await.unwrap();
    assert_eq!(users[0].username, "alice");
}

#[tokio::test]
async fn invalid_username_characters_rejected() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    let err = store.create_user("a b", &pw, false).await.unwrap_err();
    assert!(matches!(err, AuthStoreError::InvalidUsername { .. }));
}

#[tokio::test]
async fn username_over_63_chars_rejected() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    let err = store
        .create_user(&"a".repeat(64), &pw, false)
        .await
        .unwrap_err();
    assert!(matches!(err, AuthStoreError::InvalidUsername { .. }));
}

#[tokio::test]
async fn duplicate_username_rejected() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("alice", &pw, false).await.unwrap();
    let err = store.create_user("Alice", &pw, false).await.unwrap_err();
    assert!(matches!(err, AuthStoreError::UserExists(_)));
}

#[tokio::test]
async fn password_too_short_rejected() {
    let store = fresh_store();
    let pw = SecretString::new("short".to_owned());
    let err = store.create_user("alice", &pw, false).await.unwrap_err();
    assert!(matches!(err, AuthStoreError::PasswordTooShort { .. }));
}

#[tokio::test]
async fn password_too_long_rejected() {
    let store = fresh_store();
    let pw = SecretString::new("a".repeat(1025));
    let err = store.create_user("alice", &pw, false).await.unwrap_err();
    assert!(matches!(err, AuthStoreError::PasswordTooLong { .. }));
}

#[tokio::test]
async fn password_empty_rejected() {
    let store = fresh_store();
    let pw = SecretString::new(String::new());
    let err = store.create_user("alice", &pw, false).await.unwrap_err();
    assert!(matches!(err, AuthStoreError::PasswordEmpty));
}

// ── 5.2: SystemGraphAuthProvider ────────────────────────────────────────────

#[tokio::test]
async fn auth_succeeds_with_correct_password() {
    let (provider, store) = fresh_pair();
    let pw = SecretString::new("correct-horse".to_owned());
    store.create_user("alice", &pw, false).await.unwrap();

    let outcome = provider
        .authenticate("alice", "correct-horse")
        .await
        .expect("ok");
    assert!(!outcome.user_id.is_empty());
    assert!(outcome.roles.is_empty());
}

#[tokio::test]
async fn auth_rejects_wrong_password() {
    let (provider, store) = fresh_pair();
    let pw = SecretString::new("correct-horse".to_owned());
    store.create_user("alice", &pw, false).await.unwrap();

    let err = provider
        .authenticate("alice", "wrong-pass")
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::InvalidCredentials));
}

#[tokio::test]
async fn auth_rejects_unknown_user_after_dummy_verify() {
    let (provider, _) = fresh_pair();
    let start = std::time::Instant::now();
    let err = provider
        .authenticate("ghost", "anything")
        .await
        .unwrap_err();
    let elapsed = start.elapsed();
    assert!(matches!(err, AuthError::UnknownUser));
    // The dummy argon2 verify must have run — otherwise the miss path
    // would complete in microseconds.
    assert!(
        elapsed >= std::time::Duration::from_millis(10),
        "expected dummy argon2 verify, elapsed {elapsed:?}"
    );
}

// ── 5.3: mutations + last-admin protection ──────────────────────────────────

#[tokio::test]
async fn drop_user_removes_from_store() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("admin", &pw, true).await.unwrap();
    store.create_user("alice", &pw, false).await.unwrap();

    store.drop_user("alice").await.unwrap();
    let list = store.list_users().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].username, "admin");
}

#[tokio::test]
async fn drop_last_enabled_admin_rejected() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("admin", &pw, true).await.unwrap();

    let err = store.drop_user("admin").await.unwrap_err();
    assert!(matches!(err, AuthStoreError::LastAdmin));
}

#[tokio::test]
async fn set_admin_false_on_last_admin_rejected() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("admin", &pw, true).await.unwrap();

    let err = store.set_admin("admin", false).await.unwrap_err();
    assert!(matches!(err, AuthStoreError::LastAdmin));
}

#[tokio::test]
async fn set_enabled_false_on_last_admin_rejected() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("admin", &pw, true).await.unwrap();

    let err = store.set_enabled("admin", false).await.unwrap_err();
    assert!(matches!(err, AuthStoreError::LastAdmin));
}

#[tokio::test]
async fn set_password_reauth_roundtrip() {
    let (provider, store) = fresh_pair();
    let first = SecretString::new("first-password".to_owned());
    store.create_user("alice", &first, false).await.unwrap();

    let second = SecretString::new("second-password".to_owned());
    store.set_password("alice", &second).await.unwrap();

    assert!(
        provider
            .authenticate("alice", "second-password")
            .await
            .is_ok()
    );
    let err = provider
        .authenticate("alice", "first-password")
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::InvalidCredentials));
}

#[tokio::test]
async fn set_enabled_false_prevents_auth() {
    let (provider, store) = fresh_pair();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("admin", &pw, true).await.unwrap();
    store.create_user("alice", &pw, false).await.unwrap();

    store.set_enabled("alice", false).await.unwrap();
    let err = provider
        .authenticate("alice", "hunter22!x")
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::UserDisabled));
}

#[tokio::test]
async fn set_admin_promotes_and_demotes_when_not_last() {
    let store = fresh_store();
    let pw = SecretString::new("hunter22!x".to_owned());
    store.create_user("admin", &pw, true).await.unwrap();
    store.create_user("alice", &pw, false).await.unwrap();

    store.set_admin("alice", true).await.unwrap();
    let list = store.list_users().await.unwrap();
    let alice = list.iter().find(|u| u.username == "alice").expect("alice");
    assert!(alice.is_admin);

    // `admin` is no longer the last enabled admin; demoting alice is fine.
    store.set_admin("alice", false).await.unwrap();
}

#[tokio::test]
async fn authenticate_propagates_is_admin_for_admin_user() {
    let store = fresh_store();
    let pw = SecretString::new("passw0rd12".to_owned());
    store.create_user("root", &pw, true).await.unwrap();
    let provider = SystemGraphAuthProvider::from_store(Arc::clone(&store));
    let outcome = provider.authenticate("root", "passw0rd12").await.unwrap();
    assert!(outcome.is_admin);
}

#[tokio::test]
async fn authenticate_propagates_is_admin_false_for_regular_user() {
    let store = fresh_store();
    let pw = SecretString::new("passw0rd12".to_owned());
    store.create_user("alice", &pw, false).await.unwrap();
    let provider = SystemGraphAuthProvider::from_store(Arc::clone(&store));
    let outcome = provider.authenticate("alice", "passw0rd12").await.unwrap();
    assert!(!outcome.is_admin);
}

/// Borrar un usuario se lleva por delante lo que colgaba de él.
///
/// La versión de pago de esta prueba comprueba además que los permisos
/// concedidos desaparecen. Aquí queda la parte que esta edición sí puede
/// afirmar: el usuario deja de estar, y borrarlo no deja el almacén en un
/// estado que impida seguir operando con los demás.
#[tokio::test]
async fn drop_user_leaves_the_rest_of_the_store_usable() {
    let store = fresh_store();
    store
        .create_user("admin", &secret("passw0rd12"), true)
        .await
        .unwrap();
    store
        .create_user("alice", &secret("passw0rd12"), false)
        .await
        .unwrap();

    store.drop_user("alice").await.unwrap();

    let list = store.list_users().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].username, "admin");

    // El almacén sigue aceptando altas después del borrado.
    store
        .create_user("bob", &secret("passw0rd12"), false)
        .await
        .unwrap();
    assert_eq!(store.list_users().await.unwrap().len(), 2);
}

#[tokio::test]
async fn auth_timing_unknown_matches_invalid_within_bounds() {
    let (provider, store) = fresh_pair();
    let pw = SecretString::new("correct-horse".to_owned());
    store.create_user("alice", &pw, false).await.unwrap();

    // Warm up the hash cache and allocator.
    let _ = provider.authenticate("alice", "x").await;
    let _ = provider.authenticate("ghost", "x").await;

    let t1 = std::time::Instant::now();
    let _ = provider.authenticate("alice", "wrong-pass").await;
    let t_known = t1.elapsed();

    let t2 = std::time::Instant::now();
    let _ = provider.authenticate("ghost", "wrong-pass").await;
    let t_unknown = t2.elapsed();

    #[allow(clippy::cast_precision_loss)]
    let ratio = t_known.as_nanos() as f64 / t_unknown.as_nanos() as f64;
    assert!(
        (0.5..=2.0).contains(&ratio),
        "timing ratio {ratio} outside [0.5, 2.0]; known={t_known:?} unknown={t_unknown:?}"
    );
}
