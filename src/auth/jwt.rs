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
    secret: String,
    claim_mapping: ClaimMapping,
    /// Claims that MUST be present for the token to be accepted (Python/TS parity).
    require_claims: Vec<String>,
}

impl JWTAuthenticator {
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            claim_mapping: ClaimMapping::default(),
            require_claims: vec![],
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
}

#[async_trait]
impl Authenticator for JWTAuthenticator {
    async fn authenticate(&self, headers: &HashMap<String, String>) -> Option<Identity> {
        let auth_header = headers.get("authorization")?;
        let token = auth_header.strip_prefix("Bearer ")?;

        let key = DecodingKey::from_secret(self.secret.as_bytes());
        let mut validation = Validation::new(Algorithm::HS256);
        // Reject expired tokens (Python/TS parity).
        validation.validate_exp = true;
        // `sub` is not universally present and is mapped explicitly below; do not
        // require it at the jsonwebtoken layer.
        validation.required_spec_claims.clear();

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
    async fn missing_required_claim_is_rejected() {
        let auth =
            JWTAuthenticator::new(SECRET).with_require_claims(vec!["tenant".to_string()]);
        let t = token(&json!({ "sub": "bob" }));
        assert!(auth.authenticate(&bearer(&t)).await.is_none());
        let t2 = token(&json!({ "sub": "bob", "tenant": "t1" }));
        assert!(auth.authenticate(&bearer(&t2)).await.is_some());
    }
}
