//! JWT authentication for A2A.

use apcore::context::Identity;
use async_trait::async_trait;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use super::protocol::Authenticator;

#[derive(Debug, Clone)]
pub struct ClaimMapping {
    pub id_claim: String,
    pub roles_claim: String,
    /// Claim carrying the identity type (Python/TS parity; default `"type"`).
    pub type_claim: String,
    /// Claims copied into the Identity `attrs` map (Python/TS parity).
    pub attrs_claims: Vec<String>,
}

impl Default for ClaimMapping {
    fn default() -> Self {
        Self {
            id_claim: "sub".to_string(),
            roles_claim: "roles".to_string(),
            type_claim: "type".to_string(),
            attrs_claims: vec![],
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

pub struct JWTAuthenticator {
    /// Verification key material: HMAC secret for HS*, or a PEM public key for
    /// RS*/ES*. The interpretation depends on the configured algorithms.
    key: String,
    claim_mapping: ClaimMapping,
    /// Claims that MUST be present for the token to be accepted (Python/TS parity).
    require_claims: Vec<String>,
    /// Allowed JWT algorithms (default `["HS256"]`, Python/TS parity).
    algorithms: Vec<Algorithm>,
    /// Expected `aud` claim; verified when set (Python/TS parity).
    audience: Option<String>,
    /// Expected `iss` claim; verified when set (Python/TS parity).
    issuer: Option<String>,
}

impl JWTAuthenticator {
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            key: secret.into(),
            claim_mapping: ClaimMapping::default(),
            require_claims: vec![],
            algorithms: vec![Algorithm::HS256],
            audience: None,
            issuer: None,
        }
    }

    pub fn with_claim_mapping(mut self, mapping: ClaimMapping) -> Self {
        self.claim_mapping = mapping;
        self
    }

    /// Require the listed claims to be present (rejects the token otherwise).
    pub fn with_require_claims(mut self, claims: Vec<String>) -> Self {
        self.require_claims = claims;
        self
    }

    /// Set the allowed JWT algorithms (default `["HS256"]`).
    ///
    /// For asymmetric algorithms (RS256/ES256), the authenticator's key must be
    /// the PEM-encoded public key.
    pub fn with_algorithms(mut self, algorithms: Vec<Algorithm>) -> Self {
        if !algorithms.is_empty() {
            self.algorithms = algorithms;
        }
        self
    }

    /// Verify the `aud` claim against the given audience.
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }

    /// Verify the `iss` claim against the given issuer.
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Build the appropriate `DecodingKey` for the configured algorithm family.
    ///
    /// HS* algorithms treat `key` as a raw HMAC secret; RS* / PS* and ES*
    /// treat it as a PEM-encoded public key.
    fn decoding_key(&self) -> Option<DecodingKey> {
        use Algorithm::*;
        // Algorithms are validated to be a single family at decode time; pick
        // the first to decide how to interpret the key material.
        match self.algorithms.first().copied().unwrap_or(HS256) {
            HS256 | HS384 | HS512 => Some(DecodingKey::from_secret(self.key.as_bytes())),
            RS256 | RS384 | RS512 | PS256 | PS384 | PS512 => {
                DecodingKey::from_rsa_pem(self.key.as_bytes()).ok()
            }
            ES256 | ES384 => DecodingKey::from_ec_pem(self.key.as_bytes()).ok(),
            EdDSA => DecodingKey::from_ed_pem(self.key.as_bytes()).ok(),
        }
    }
}

#[async_trait]
impl Authenticator for JWTAuthenticator {
    async fn authenticate(&self, headers: &HashMap<String, String>) -> Option<Identity> {
        let auth_header = headers.get("authorization")?;
        let token = auth_header.strip_prefix("Bearer ")?;

        let key = self.decoding_key()?;
        let primary_alg = self.algorithms.first().copied().unwrap_or(Algorithm::HS256);
        let mut validation = Validation::new(primary_alg);
        validation.algorithms = self.algorithms.clone();
        // Reject expired tokens (Python/TS parity).
        validation.validate_exp = true;
        // `sub` is not universally present and is mapped explicitly below; do not
        // require it at the jsonwebtoken layer.
        validation.required_spec_claims.clear();
        // Verify aud/iss only when configured (Python/TS parity).
        match &self.audience {
            Some(aud) => validation.set_audience(&[aud]),
            None => validation.validate_aud = false,
        }
        if let Some(iss) = &self.issuer {
            validation.set_issuer(&[iss]);
        }

        let data = decode::<Claims>(token, &key, &validation).ok()?;
        let claims = data.claims.extra;
        let mapping = &self.claim_mapping;

        // Enforce required claims (Python/TS parity).
        for required in &self.require_claims {
            if !claims.contains_key(required) {
                return None;
            }
        }

        let id = claims
            .get(&mapping.id_claim)
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "anonymous".to_string());

        let roles: Vec<String> = claims
            .get(&mapping.roles_claim)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let identity_type = claims
            .get(&mapping.type_claim)
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "jwt".to_string());

        let mut attrs: HashMap<String, Value> = HashMap::new();
        for attr_claim in &mapping.attrs_claims {
            if let Some(v) = claims.get(attr_claim) {
                attrs.insert(attr_claim.clone(), v.clone());
            }
        }

        Some(Identity::new(id, identity_type, roles, attrs))
    }

    fn security_schemes(&self) -> Option<Value> {
        Some(serde_json::json!({
            "bearer": {
                "type": "http",
                "scheme": "bearer",
                "bearerFormat": "JWT"
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const SECRET: &str = "test-secret";

    fn token(claims: &Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap()
    }

    fn bearer(token: &str) -> HashMap<String, String> {
        let mut h = HashMap::new();
        h.insert("authorization".to_string(), format!("Bearer {token}"));
        h
    }

    #[tokio::test]
    async fn valid_token_authenticates() {
        let auth = JWTAuthenticator::new(SECRET);
        let t = token(&json!({ "sub": "alice", "roles": ["admin"] }));
        let identity = auth.authenticate(&bearer(&t)).await.unwrap();
        assert_eq!(identity.id(), "alice");
        assert_eq!(identity.roles(), &["admin".to_string()]);
    }

    #[tokio::test]
    async fn expired_token_is_rejected() {
        let auth = JWTAuthenticator::new(SECRET);
        // `exp` in the past (well before now) must be rejected.
        let t = token(&json!({ "sub": "alice", "exp": 1_000_000_000u64 }));
        assert!(auth.authenticate(&bearer(&t)).await.is_none());
    }

    #[tokio::test]
    async fn type_and_attrs_claims_are_mapped() {
        let auth = JWTAuthenticator::new(SECRET).with_claim_mapping(ClaimMapping {
            attrs_claims: vec!["org".to_string(), "tenant".to_string()],
            ..ClaimMapping::default()
        });
        let t = token(&json!({
            "sub": "bob",
            "type": "service",
            "org": "acme",
            "tenant": "t1",
        }));
        let identity = auth.authenticate(&bearer(&t)).await.unwrap();
        assert_eq!(identity.id(), "bob");
        assert_eq!(identity.identity_type(), "service");
        assert_eq!(identity.get_attr("org"), Some(&json!("acme")));
        assert_eq!(identity.get_attr("tenant"), Some(&json!("t1")));
    }

    #[tokio::test]
    async fn correct_audience_and_issuer_accepted() {
        let auth = JWTAuthenticator::new(SECRET)
            .with_audience("my-aud")
            .with_issuer("my-iss");
        let t = token(&json!({
            "sub": "alice",
            "aud": "my-aud",
            "iss": "my-iss",
        }));
        assert!(auth.authenticate(&bearer(&t)).await.is_some());
    }

    #[tokio::test]
    async fn wrong_audience_rejected() {
        let auth = JWTAuthenticator::new(SECRET).with_audience("expected-aud");
        let t = token(&json!({ "sub": "alice", "aud": "other-aud" }));
        assert!(auth.authenticate(&bearer(&t)).await.is_none());
    }

    #[tokio::test]
    async fn wrong_issuer_rejected() {
        let auth = JWTAuthenticator::new(SECRET).with_issuer("expected-iss");
        let t = token(&json!({ "sub": "alice", "iss": "other-iss" }));
        assert!(auth.authenticate(&bearer(&t)).await.is_none());
    }

    #[tokio::test]
    async fn audience_not_required_when_unset() {
        // No audience configured: a token carrying an `aud` claim is still fine.
        let auth = JWTAuthenticator::new(SECRET);
        let t = token(&json!({ "sub": "alice", "aud": "anything" }));
        assert!(auth.authenticate(&bearer(&t)).await.is_some());
    }

    #[tokio::test]
    async fn algorithm_selection_rejects_other_family() {
        // Token signed with HS256, but only HS384 allowed -> rejected.
        let auth = JWTAuthenticator::new(SECRET).with_algorithms(vec![Algorithm::HS384]);
        let t = token(&json!({ "sub": "alice" }));
        assert!(auth.authenticate(&bearer(&t)).await.is_none());
    }

    #[tokio::test]
    async fn missing_required_claim_is_rejected() {
        let auth =
            JWTAuthenticator::new(SECRET).with_require_claims(vec!["tenant".to_string()]);
        let t = token(&json!({ "sub": "bob" }));
        assert!(auth.authenticate(&bearer(&t)).await.is_none());
        let t2 = token(&json!({ "sub": "bob", "tenant": "t1" }));
        assert!(auth.authenticate(&bearer(&t2)).await.is_some());
    }
}
