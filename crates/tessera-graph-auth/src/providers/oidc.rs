// Copyright 2026 BelowZero Security OU. All rights reserved.

//! OIDC authentication provider for JWT validation (Keycloak, Okta, Azure AD).
//!
//! The client obtains a JWT from the identity provider externally and passes it
//! as the `credential` field in the login message. This provider validates the
//! JWT signature, expiry, issuer, and audience, then extracts group claims.

use std::collections::HashMap;

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use tokio::sync::RwLock;

use crate::error::{AuthError, Result};
use crate::providers::{ExternalAuthProvider, ExternalUserInfo};

/// Configuration for the OIDC authentication provider.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// Expected issuer (`iss` claim).
    pub issuer: String,
    /// Expected audience (`aud` claim).
    pub audience: String,
    /// URL to fetch the JWKS (JSON Web Key Set) document.
    pub jwks_url: String,
    /// JWT claim name containing group/role information.
    pub group_claim: String,
    /// Mapping of OIDC group names to internal role names.
    pub group_mapping: HashMap<String, String>,
}

impl OidcConfig {
    /// Load OIDC configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::ConfigError` if any required variable is missing.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            issuer: required_env("TESSERA_OIDC_ISSUER")?,
            audience: required_env("TESSERA_OIDC_AUDIENCE")?,
            jwks_url: required_env("TESSERA_OIDC_JWKS_URL")?,
            group_claim: optional_env("TESSERA_OIDC_GROUP_CLAIM")
                .unwrap_or_else(|| "groups".to_owned()),
            group_mapping: optional_env("TESSERA_OIDC_GROUP_MAPPING")
                .map_or_else(HashMap::new, |s| {
                    crate::providers::group_mapping::parse_group_mapping(&s)
                }),
        })
    }
}

/// A JWKS (JSON Web Key Set) document containing public keys for JWT verification.
#[derive(Debug, Clone)]
pub struct JwksDocument {
    /// The raw JWKS as parsed by `jsonwebtoken`.
    pub keys: jsonwebtoken::jwk::JwkSet,
}

/// Abstraction over JWKS fetching, enabling mock injection in tests.
pub trait JwksFetcher: Send + Sync {
    /// Fetch the current JWKS document from the identity provider.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::ProviderUnavailable` if the JWKS endpoint is unreachable.
    fn fetch(&self) -> impl std::future::Future<Output = Result<JwksDocument>> + Send;
}

/// OIDC authentication provider, generic over the JWKS fetcher for testability.
pub struct OidcAuthProvider<F: JwksFetcher> {
    config: OidcConfig,
    fetcher: F,
    jwks_cache: RwLock<Option<JwksDocument>>,
}

impl<F: JwksFetcher> OidcAuthProvider<F> {
    /// Create a provider with an injectable JWKS fetcher (for testing).
    pub fn with_fetcher(config: OidcConfig, fetcher: F) -> Self {
        Self {
            config,
            fetcher,
            jwks_cache: RwLock::new(None),
        }
    }

    /// Get JWKS from cache or fetch if not cached.
    async fn get_jwks(&self) -> Result<JwksDocument> {
        {
            let cache = self.jwks_cache.read().await;
            if let Some(ref jwks) = *cache {
                return Ok(jwks.clone());
            }
        }
        self.refresh_jwks().await
    }

    /// Force-refresh the JWKS cache.
    #[allow(clippy::significant_drop_tightening)]
    async fn refresh_jwks(&self) -> Result<JwksDocument> {
        let jwks = self.fetcher.fetch().await?;
        let mut cache = self.jwks_cache.write().await;
        *cache = Some(jwks.clone());
        Ok(jwks)
    }

    /// Find the decoding key for a given `kid` in the JWKS, with one retry on miss.
    async fn find_key(&self, kid: &str) -> Result<(DecodingKey, Algorithm)> {
        // Try cached JWKS first
        let jwks = self.get_jwks().await?;
        if let Some(result) = Self::extract_key(&jwks, kid) {
            return Ok(result);
        }

        // Cache miss — force refresh and retry once
        let jwks = self.refresh_jwks().await?;
        Self::extract_key(&jwks, kid).ok_or(AuthError::InvalidCredentials)
    }

    /// Extract a `DecodingKey` + `Algorithm` from a JWKS for a specific `kid`.
    fn extract_key(jwks: &JwksDocument, kid: &str) -> Option<(DecodingKey, Algorithm)> {
        let jwk = jwks.keys.find(kid)?;

        let algorithm = jwk
            .common
            .key_algorithm
            .and_then(Self::key_algorithm_to_algorithm)?;

        let key = DecodingKey::from_jwk(jwk).ok()?;
        Some((key, algorithm))
    }

    /// Map `jsonwebtoken::jwk::KeyAlgorithm` to `jsonwebtoken::Algorithm`.
    /// Rejects symmetric algorithms (HS256, etc.) and `none`.
    const fn key_algorithm_to_algorithm(ka: jsonwebtoken::jwk::KeyAlgorithm) -> Option<Algorithm> {
        match ka {
            jsonwebtoken::jwk::KeyAlgorithm::RS256 => Some(Algorithm::RS256),
            jsonwebtoken::jwk::KeyAlgorithm::RS384 => Some(Algorithm::RS384),
            jsonwebtoken::jwk::KeyAlgorithm::RS512 => Some(Algorithm::RS512),
            jsonwebtoken::jwk::KeyAlgorithm::ES256 => Some(Algorithm::ES256),
            jsonwebtoken::jwk::KeyAlgorithm::ES384 => Some(Algorithm::ES384),
            jsonwebtoken::jwk::KeyAlgorithm::PS256 => Some(Algorithm::PS256),
            jsonwebtoken::jwk::KeyAlgorithm::PS384 => Some(Algorithm::PS384),
            jsonwebtoken::jwk::KeyAlgorithm::PS512 => Some(Algorithm::PS512),
            // Reject HS* and EdDSA (not commonly used in OIDC), and any unknown
            _ => None,
        }
    }
}

/// JWT claims structure for extraction.
#[derive(Debug, serde::Deserialize)]
struct JwtClaims {
    sub: Option<String>,
    #[allow(dead_code)]
    iss: Option<String>,
    #[allow(dead_code)]
    aud: Option<serde_json::Value>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

impl<F: JwksFetcher + 'static> ExternalAuthProvider for OidcAuthProvider<F> {
    fn authenticate(
        &self,
        username: &str,
        credential: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExternalUserInfo>> + Send + '_>>
    {
        let username = username.to_owned();
        let credential = credential.to_owned();
        Box::pin(self.do_authenticate(username, credential))
    }

    fn provider_name(&self) -> &'static str {
        "oidc"
    }
}

impl<F: JwksFetcher> OidcAuthProvider<F> {
    #[allow(clippy::significant_drop_tightening)]
    async fn do_authenticate(
        &self,
        username: String,
        credential: String,
    ) -> Result<ExternalUserInfo> {
        let username = &username;
        let credential = &credential;
        // Step 1: Decode header to get kid (unverified)
        let header =
            jsonwebtoken::decode_header(credential).map_err(|_| AuthError::InvalidCredentials)?;

        let kid = header.kid.ok_or(AuthError::InvalidCredentials)?;

        // Step 2: Find the matching key
        let (key, algorithm) = self.find_key(&kid).await?;

        // Step 3: Validate JWT
        let mut validation = Validation::new(algorithm);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.audience]);
        validation.validate_exp = true;
        // Reject alg=none by only allowing the specific algorithm from the JWK
        validation.algorithms = vec![algorithm];

        let token_data = jsonwebtoken::decode::<JwtClaims>(credential, &key, &validation)
            .map_err(|_| AuthError::InvalidCredentials)?;

        let claims = token_data.claims;

        // Step 4: Extract username (prefer sub claim, fall back to provided username)
        let resolved_username = claims
            .sub
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| username.to_owned());

        // Step 5: Extract groups from configured claim
        let groups = claims
            .extra
            .get(&self.config.group_claim)
            .and_then(|v| {
                if let serde_json::Value::Array(arr) = v {
                    Some(
                        arr.iter()
                            .filter_map(|item| item.as_str().map(String::from))
                            .collect::<Vec<String>>(),
                    )
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // Step 6: Extract email if present
        let email = claims
            .extra
            .get("email")
            .and_then(|v| v.as_str().map(String::from));

        let display_name = claims
            .extra
            .get("name")
            .and_then(|v| v.as_str().map(String::from));

        Ok(ExternalUserInfo {
            username: resolved_username,
            groups,
            email,
            display_name,
        })
    }
}

/// Read a required environment variable.
fn required_env(key: &str) -> Result<String> {
    std::env::var(key)
        .map_err(|_| AuthError::ConfigError(format!("missing required env var: {key}")))
}

/// Read an optional environment variable.
fn optional_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use std::sync::atomic::{AtomicU32, Ordering};

    // --- Test helpers ---

    /// Generate an RSA key pair and return (key, jwks, kid).
    fn generate_test_keys() -> (EncodingKey, JwksDocument, String) {
        // Use the jsonwebtoken crate's test-friendly approach: generate RSA PEM
        // We use a pre-generated small RSA key for test speed
        let rsa_private = include_str!("../../tests/fixtures/test_rsa_private.pem");
        let rsa_public = include_str!("../../tests/fixtures/test_rsa_public.pem");

        let encoding_key = EncodingKey::from_rsa_pem(rsa_private.as_bytes()).expect("encoding key"); // OK: test

        let kid = "test-kid-001".to_owned();

        // Build JWKS from the public key
        let n_b64 = extract_rsa_n_from_pem(rsa_public);
        let e_b64 = "AQAB".to_owned(); // Standard RSA public exponent

        let jwk_json = format!(
            r#"{{"keys":[{{"kty":"RSA","kid":"{kid}","alg":"RS256","use":"sig","n":"{n_b64}","e":"{e_b64}"}}]}}"#
        );
        let jwk_set: jsonwebtoken::jwk::JwkSet =
            serde_json::from_str(&jwk_json).expect("parse jwks"); // OK: test

        let jwks = JwksDocument { keys: jwk_set };

        (encoding_key, jwks, kid)
    }

    /// Extract the RSA modulus (n) from a PEM public key as base64url.
    fn extract_rsa_n_from_pem(pem: &str) -> String {
        use base64ct::{Base64UrlUnpadded, Encoding};
        // Parse the PEM to get the raw DER
        let der = pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<String>();
        let der_bytes = base64ct::Base64::decode_vec(&der).expect("base64 decode"); // OK: test
        // For RSA public keys in PKCS#1 format, the modulus starts at a known offset
        // We use a simplified extraction — find the large integer (>= 256 bytes)
        // In practice, the modulus is the first large ASN.1 INTEGER
        let n_bytes = extract_first_large_integer(&der_bytes);
        Base64UrlUnpadded::encode_string(&n_bytes)
    }

    /// Extract the first large integer (>128 bytes) from DER-encoded data.
    fn extract_first_large_integer(der: &[u8]) -> Vec<u8> {
        let mut i = 0;
        while i + 4 < der.len() {
            if der[i] == 0x02 {
                // ASN.1 INTEGER tag
                let (len, header_len) = parse_asn1_length(&der[i + 1..]);
                if len >= 128 {
                    let start = i + 1 + header_len;
                    let end = start + len;
                    if end <= der.len() {
                        let mut n = der[start..end].to_vec();
                        // Remove leading zero byte if present (ASN.1 sign byte)
                        if n.first() == Some(&0) {
                            n.remove(0);
                        }
                        return n;
                    }
                }
            }
            i += 1;
        }
        vec![]
    }

    /// Parse ASN.1 length encoding, returning (length, header bytes consumed).
    fn parse_asn1_length(data: &[u8]) -> (usize, usize) {
        if data.is_empty() {
            return (0, 0);
        }
        if data[0] < 0x80 {
            (data[0] as usize, 1)
        } else {
            let num_bytes = (data[0] & 0x7F) as usize;
            let mut len = 0usize;
            for b in &data[1..=num_bytes] {
                len = (len << 8) | (*b as usize);
            }
            (len, 1 + num_bytes)
        }
    }

    fn sign_jwt(encoding_key: &EncodingKey, kid: &str, claims: &serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_owned());
        jsonwebtoken::encode(&header, claims, encoding_key).expect("sign JWT") // OK: test
    }

    fn test_config() -> OidcConfig {
        OidcConfig {
            issuer: "https://idp.example.com".to_owned(),
            audience: "tessera-graph".to_owned(),
            jwks_url: "https://idp.example.com/.well-known/jwks.json".to_owned(),
            group_claim: "groups".to_owned(),
            group_mapping: HashMap::new(),
        }
    }

    fn valid_claims() -> serde_json::Value {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time") // OK: test
            .as_secs();
        serde_json::json!({
            "sub": "alice",
            "iss": "https://idp.example.com",
            "aud": "tessera-graph",
            "exp": now + 300,
            "iat": now,
            "groups": ["admin", "developers"],
            "email": "alice@example.com",
            "name": "Alice Liddell"
        })
    }

    struct MockJwksFetcher {
        document: JwksDocument,
    }

    impl JwksFetcher for MockJwksFetcher {
        async fn fetch(&self) -> Result<JwksDocument> {
            Ok(self.document.clone())
        }
    }

    struct CountingJwksFetcher {
        documents: Vec<JwksDocument>,
        call_count: AtomicU32,
    }

    impl JwksFetcher for CountingJwksFetcher {
        async fn fetch(&self) -> Result<JwksDocument> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst) as usize;
            self.documents
                .get(idx)
                .cloned()
                .ok_or_else(|| AuthError::ProviderUnavailable("no more documents".to_owned()))
        }
    }

    // --- Tests ---

    #[test]
    fn oidc_config_roundtrip() {
        let cfg = test_config();
        assert_eq!(cfg.issuer, "https://idp.example.com");
        assert_eq!(cfg.group_claim, "groups");
    }

    #[tokio::test]
    async fn oidc_authenticate_valid_rs256_jwt() {
        let (encoding_key, jwks, kid) = generate_test_keys();
        let claims = valid_claims();
        let jwt = sign_jwt(&encoding_key, &kid, &claims);

        let fetcher = MockJwksFetcher { document: jwks };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);

        let info = provider.authenticate("alice", &jwt).await.expect("auth ok"); // OK: test
        assert_eq!(info.username, "alice");
        assert_eq!(info.email.as_deref(), Some("alice@example.com"));
        assert_eq!(info.display_name.as_deref(), Some("Alice Liddell"));
        assert!(info.groups.contains(&"admin".to_owned()));
        assert!(info.groups.contains(&"developers".to_owned()));
    }

    #[tokio::test]
    async fn oidc_expired_token_returns_invalid_credentials() {
        let (encoding_key, jwks, kid) = generate_test_keys();
        let mut claims = valid_claims();
        claims["exp"] = serde_json::json!(1); // expired in 1970
        let jwt = sign_jwt(&encoding_key, &kid, &claims);

        let fetcher = MockJwksFetcher { document: jwks };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);

        let err = provider
            .authenticate("alice", &jwt)
            .await
            .expect_err("fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn oidc_wrong_issuer_returns_invalid_credentials() {
        let (encoding_key, jwks, kid) = generate_test_keys();
        let mut claims = valid_claims();
        claims["iss"] = serde_json::json!("https://wrong-issuer.example.com");
        let jwt = sign_jwt(&encoding_key, &kid, &claims);

        let fetcher = MockJwksFetcher { document: jwks };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);

        let err = provider
            .authenticate("alice", &jwt)
            .await
            .expect_err("fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn oidc_wrong_audience_returns_invalid_credentials() {
        let (encoding_key, jwks, kid) = generate_test_keys();
        let mut claims = valid_claims();
        claims["aud"] = serde_json::json!("wrong-audience");
        let jwt = sign_jwt(&encoding_key, &kid, &claims);

        let fetcher = MockJwksFetcher { document: jwks };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);

        let err = provider
            .authenticate("alice", &jwt)
            .await
            .expect_err("fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn oidc_invalid_jwt_string_returns_invalid_credentials() {
        let (_, jwks, _) = generate_test_keys();
        let fetcher = MockJwksFetcher { document: jwks };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);

        let err = provider
            .authenticate("alice", "not-a-jwt")
            .await
            .expect_err("fail"); // OK: test
        assert!(matches!(err, AuthError::InvalidCredentials));
    }

    #[tokio::test]
    async fn oidc_provider_name() {
        let (_, jwks, _) = generate_test_keys();
        let fetcher = MockJwksFetcher { document: jwks };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);
        assert_eq!(provider.provider_name(), "oidc");
    }

    #[tokio::test]
    async fn oidc_refreshes_jwks_on_unknown_kid() {
        let (encoding_key, jwks, kid) = generate_test_keys();
        let claims = valid_claims();
        let jwt = sign_jwt(&encoding_key, &kid, &claims);

        // First fetch returns empty JWKS, second returns real JWKS
        let empty_jwks = JwksDocument {
            keys: serde_json::from_str(r#"{"keys":[]}"#).expect("empty jwks"), // OK: test
        };

        let fetcher = CountingJwksFetcher {
            documents: vec![empty_jwks, jwks],
            call_count: AtomicU32::new(0),
        };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);

        let info = provider
            .authenticate("alice", &jwt)
            .await
            .expect("auth ok after refresh"); // OK: test
        assert_eq!(info.username, "alice");

        // Should have fetched twice (initial miss + refresh)
        assert_eq!(provider.fetcher.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn oidc_missing_sub_falls_back_to_provided_username() {
        let (encoding_key, jwks, kid) = generate_test_keys();
        let mut claims = valid_claims();
        claims.as_object_mut().expect("obj").remove("sub"); // OK: test
        let jwt = sign_jwt(&encoding_key, &kid, &claims);

        let fetcher = MockJwksFetcher { document: jwks };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);

        let info = provider
            .authenticate("fallback-user", &jwt)
            .await
            .expect("ok"); // OK: test
        assert_eq!(info.username, "fallback-user");
    }

    #[tokio::test]
    async fn oidc_no_group_claim_returns_empty_groups() {
        let (encoding_key, jwks, kid) = generate_test_keys();
        let mut claims = valid_claims();
        claims.as_object_mut().expect("obj").remove("groups"); // OK: test
        let jwt = sign_jwt(&encoding_key, &kid, &claims);

        let fetcher = MockJwksFetcher { document: jwks };
        let provider = OidcAuthProvider::with_fetcher(test_config(), fetcher);

        let info = provider.authenticate("alice", &jwt).await.expect("ok"); // OK: test
        assert!(info.groups.is_empty());
    }
}
