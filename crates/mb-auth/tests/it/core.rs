#![cfg(test)]
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use http::HeaderMap;
use http::header::{AUTHORIZATION, COOKIE};
use mb_auth::{
    API_KEY_PREFIX, ApiKeys, AuthError, Authenticate, Authenticator, KeyScope, NewKeyRequest,
    Permission, Principal, PrincipalKind, Role, RoleMapping, SESSION_COOKIE, SessionManager,
    generate_key, hash_secret, parse_token,
};
use mb_store::{ControlStore, connect};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

async fn store() -> Arc<dyn ControlStore> {
    connect("sqlite::memory:").await.unwrap()
}

fn claims(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => panic!("not an object"),
    }
}

fn user(roles: &[(&str, Role)], sysadmin: bool) -> Principal {
    Principal {
        subject: "alice".into(),
        display_name: "Alice".into(),
        kind: PrincipalKind::User,
        sysadmin,
        roles: roles.iter().map(|(w, r)| (w.to_string(), *r)).collect(),
    }
}

fn req(name: &str, role: Role) -> NewKeyRequest {
    NewKeyRequest {
        name: name.into(),
        role,
        scopes: vec![],
        expires_at: None,
        sysadmin: false,
    }
}

fn bearer(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    h
}

fn authenticator(store: Arc<dyn ControlStore>) -> Authenticator {
    Authenticator {
        api_keys: ApiKeys::new(store.clone()),
        sessions: Some(SessionManager::new(store, Duration::from_secs(3600), false)),
        bearer: None,
    }
}

fn sha_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn role_order_and_permissions() {
    use Permission::*;
    use Role::*;
    assert!(Viewer < Editor && Editor < Admin && Admin < Owner);
    assert_eq!(Read.min_role(), Viewer);
    assert_eq!(Edit.min_role(), Editor);
    assert_eq!(ManageProfiles.min_role(), Admin);
    assert_eq!(ManageApiKeys.min_role(), Admin);
    assert_eq!(ManageWorkspace.min_role(), Owner);
    let matrix: [(Role, [bool; 5]); 4] = [
        (Viewer, [true, false, false, false, false]),
        (Editor, [true, true, false, false, false]),
        (Admin, [true, true, true, true, false]),
        (Owner, [true, true, true, true, true]),
    ];
    let perms = [Read, Edit, ManageProfiles, ManageApiKeys, ManageWorkspace];
    for (role, expected) in matrix {
        for (p, want) in perms.iter().zip(expected) {
            assert_eq!(role.allows(*p), want, "{role:?} {p:?}");
        }
        assert_eq!(Role::parse(role.as_str()), Some(role));
        assert_eq!(
            serde_json::to_value(role).unwrap(),
            json!(role.as_str()),
            "snake_case wire format"
        );
    }
    assert_eq!(Role::parse("superuser"), None);
    assert_eq!(Owner.as_str(), "owner");
}

#[test]
fn sysadmin_is_owner_everywhere() {
    let p = user(&[("default", Role::Viewer)], true);
    assert_eq!(p.role_in("default"), Some(Role::Owner));
    assert_eq!(p.role_in("anything"), Some(Role::Owner));
    assert!(p.can("anything", Permission::ManageWorkspace));

    let local = Principal::local();
    assert_eq!(local.subject, "local");
    assert_eq!(local.display_name, "Local user");
    assert_eq!(local.kind, PrincipalKind::Local);
    assert!(local.sysadmin);
    assert_eq!(local.role_in("x"), Some(Role::Owner));

    let plain = user(&[("default", Role::Editor)], false);
    assert_eq!(plain.role_in("other"), None);
    assert!(!plain.can("other", Permission::Read));
    assert!(plain.can("default", Permission::Edit));
    assert!(!plain.can("default", Permission::ManageProfiles));
}

#[test]
fn claims_map_to_roles() {
    let m = RoleMapping::default();
    assert_eq!(m.roles_claim, "groups");
    assert_eq!(m.group_prefix, "mb");
    assert!(m.sysadmin_subjects.is_empty());
    let p = m
        .principal(&claims(json!({
            "sub": "u1",
            "email": "u1@example.com",
            "groups": ["mb:default:editor", "mb:team-a:viewer", "mb:team-a:admin", "other:x"]
        })))
        .unwrap();
    assert_eq!(p.subject, "u1");
    assert_eq!(p.display_name, "u1@example.com");
    assert_eq!(p.kind, PrincipalKind::User);
    assert!(!p.sysadmin);
    let want: BTreeMap<String, Role> = [
        ("default".to_string(), Role::Editor),
        ("team-a".to_string(), Role::Admin),
    ]
    .into();
    assert_eq!(p.roles, want);

    // display_name precedence
    let p = m
        .principal(&claims(
            json!({"sub": "u2", "name": "N", "preferred_username": "pu"}),
        ))
        .unwrap();
    assert_eq!(p.display_name, "N");
    let p = m
        .principal(&claims(
            json!({"sub": "u2", "preferred_username": "pu", "email": "e"}),
        ))
        .unwrap();
    assert_eq!(p.display_name, "pu");
    let p = m.principal(&claims(json!({"sub": "u2"}))).unwrap();
    assert_eq!(p.display_name, "u2");
    assert!(p.roles.is_empty());
}

#[test]
fn space_separated_roles_claim() {
    let p = RoleMapping::default()
        .principal(&claims(json!({
            "sub": "u1",
            "groups": "mb:default:viewer  mb:default:owner mb:ops:editor"
        })))
        .unwrap();
    assert_eq!(p.role_in("default"), Some(Role::Owner));
    assert_eq!(p.role_in("ops"), Some(Role::Editor));
}

#[test]
fn custom_prefix_and_claim() {
    let m = RoleMapping {
        roles_claim: "roles".into(),
        group_prefix: "mock".into(),
        sysadmin_subjects: vec![],
    };
    let p = m
        .principal(&claims(json!({
            "sub": "u1",
            "groups": ["mock:default:owner"],
            "roles": ["mock:default:editor", "mb:default:owner", "mock:x:bogus"]
        })))
        .unwrap();
    let want: BTreeMap<String, Role> = [("default".to_string(), Role::Editor)].into();
    assert_eq!(p.roles, want);
}

#[test]
fn sysadmin_from_group_and_subject() {
    let m = RoleMapping {
        sysadmin_subjects: vec!["root-sub".into()],
        ..RoleMapping::default()
    };
    let p = m
        .principal(&claims(json!({"sub": "u1", "groups": ["mb:sysadmin"]})))
        .unwrap();
    assert!(p.sysadmin);
    let p = m.principal(&claims(json!({"sub": "root-sub"}))).unwrap();
    assert!(p.sysadmin);
    let p = m
        .principal(&claims(json!({"sub": "u1", "groups": ["other:sysadmin"]})))
        .unwrap();
    assert!(!p.sysadmin);
}

#[test]
fn missing_sub_rejected() {
    let m = RoleMapping::default();
    for c in [json!({"name": "x"}), json!({"sub": ""}), json!({"sub": 5})] {
        assert!(matches!(
            m.principal(&claims(c)),
            Err(AuthError::Unauthenticated(_))
        ));
    }
}

#[test]
fn token_format_round_trips() {
    let g = generate_key();
    assert!(g.token.starts_with(API_KEY_PREFIX));
    assert_eq!(g.key_id.len(), 12);
    assert!(
        g.key_id
            .chars()
            .all(|c| "abcdefghijklmnopqrstuvwxyz234567".contains(c))
    );
    let secret = &g.token[API_KEY_PREFIX.len() + 13..];
    assert_eq!(secret.len(), 43);
    assert_eq!(g.token, format!("mbk_{}_{secret}", g.key_id));
    let (id, bytes) = parse_token(&g.token).unwrap();
    assert_eq!(id, g.key_id);
    assert_eq!(hash_secret(&bytes), g.secret_hash);
    assert_eq!(g.secret_hash.len(), 64);
    assert_eq!(URL_SAFE_NO_PAD.decode(secret).unwrap(), bytes.to_vec());

    let other = generate_key();
    assert_ne!(other.key_id, g.key_id);
    assert_ne!(other.token, g.token);

    let bad = [
        String::new(),
        g.token[4..].to_string(),                       // missing prefix
        format!("mbx_{}", &g.token[4..]),               // wrong prefix
        g.token[..g.token.len() - 1].to_string(),       // short secret
        format!("{}A", g.token),                        // long secret
        format!("mbk_{}_{secret}", &g.key_id[..11]),    // short id
        format!("mbk_{}X_{secret}", &g.key_id[..11]),   // id outside alphabet
        format!("mbk_{}-{secret}", g.key_id),           // wrong separator
        format!("mbk_{}_{}", g.key_id, "!".repeat(43)), // non-base64
    ];
    for t in bad {
        assert!(parse_token(&t).is_none(), "{t}");
    }
}

#[tokio::test]
async fn api_key_verify_ok_and_last_used_throttled() {
    let s = store().await;
    let keys = ApiKeys::new(s.clone());
    let creator = user(&[("default", Role::Admin)], false);
    let (rec, token) = keys
        .create(&creator, "default", req("ci", Role::Editor))
        .await
        .unwrap();
    assert_eq!(rec.created_by, "alice");
    assert_eq!(rec.scopes, vec!["admin".to_string()]);
    assert_eq!(rec.role, "editor");
    assert!(rec.last_used_at.is_none());
    assert!(!token.contains(&rec.secret_hash));

    let k = keys.verify(&token, KeyScope::Admin).await.unwrap();
    assert_eq!(k.key_id, rec.key_id);
    let first = s.get_api_key(&rec.key_id).await.unwrap().unwrap();
    let used = first.last_used_at.unwrap();
    keys.verify(&token, KeyScope::Admin).await.unwrap();
    let second = s.get_api_key(&rec.key_id).await.unwrap().unwrap();
    assert_eq!(second.last_used_at, Some(used));

    let p = ApiKeys::principal(&k);
    assert_eq!(p.kind, PrincipalKind::ApiKey);
    assert_eq!(p.subject, format!("api_key:{}", rec.key_id));
    assert_eq!(p.display_name, "ci");
    assert!(!p.sysadmin);
    assert_eq!(p.role_in("default"), Some(Role::Editor));
    assert_eq!(p.role_in("other"), None);

    let auth = authenticator(s);
    let p = auth.authenticate(&bearer(&token)).await.unwrap().unwrap();
    assert_eq!(p.subject, format!("api_key:{}", rec.key_id));
}

#[tokio::test]
async fn revoked_key_401() {
    let s = store().await;
    let keys = ApiKeys::new(s.clone());
    let (rec, token) = keys
        .create(&Principal::local(), "default", req("k", Role::Viewer))
        .await
        .unwrap();
    assert!(s.revoke_api_key("default", &rec.key_id).await.unwrap());
    assert!(matches!(
        keys.verify(&token, KeyScope::Admin).await,
        Err(AuthError::Unauthenticated(_))
    ));
}

#[tokio::test]
async fn expired_key_401() {
    let s = store().await;
    let keys = ApiKeys::new(s);
    let mut r = req("k", Role::Viewer);
    r.expires_at = Some(Utc::now() - chrono::Duration::seconds(5));
    let (_, token) = keys
        .create(&Principal::local(), "default", r)
        .await
        .unwrap();
    assert!(matches!(
        keys.verify(&token, KeyScope::Admin).await,
        Err(AuthError::Unauthenticated(_))
    ));
}

#[tokio::test]
async fn wrong_secret_401() {
    let s = store().await;
    let keys = ApiKeys::new(s);
    let (rec, _) = keys
        .create(&Principal::local(), "default", req("k", Role::Viewer))
        .await
        .unwrap();
    let other = generate_key();
    let secret = &other.token[API_KEY_PREFIX.len() + 13..];
    let forged = format!("mbk_{}_{secret}", rec.key_id);
    assert!(matches!(
        keys.verify(&forged, KeyScope::Admin).await,
        Err(AuthError::Unauthenticated(_))
    ));
    // Unknown id and garbage too.
    for t in [other.token.as_str(), "mbk_garbage"] {
        assert!(matches!(
            keys.verify(t, KeyScope::Admin).await,
            Err(AuthError::Unauthenticated(_))
        ));
    }
}

#[tokio::test]
async fn data_plane_key_cannot_authenticate_admin() {
    let s = store().await;
    let keys = ApiKeys::new(s.clone());
    let mut r = req("dp", Role::Viewer);
    r.scopes = vec![KeyScope::DataPlane];
    let (rec, token) = keys
        .create(&Principal::local(), "default", r)
        .await
        .unwrap();
    assert_eq!(rec.scopes, vec!["data-plane".to_string()]);
    assert!(keys.verify(&token, KeyScope::DataPlane).await.is_ok());
    let auth = authenticator(s);
    assert!(matches!(
        auth.authenticate(&bearer(&token)).await,
        Err(AuthError::Unauthenticated(_))
    ));
    assert_eq!(
        serde_json::to_value(KeyScope::DataPlane).unwrap(),
        json!("data-plane")
    );
}

#[tokio::test]
async fn api_key_principal_cannot_create_keys() {
    let s = store().await;
    let keys = ApiKeys::new(s);
    let (_, token) = keys
        .create(
            &Principal::local(),
            "default",
            req("owner-key", Role::Owner),
        )
        .await
        .unwrap();
    let k = keys.verify(&token, KeyScope::Admin).await.unwrap();
    let key_principal = ApiKeys::principal(&k);
    assert_eq!(key_principal.kind, PrincipalKind::ApiKey);
    assert!(matches!(
        keys.create(&key_principal, "default", req("k", Role::Viewer))
            .await,
        Err(AuthError::Forbidden(_))
    ));
    // Users and the Local principal still can.
    let owner = user(&[("default", Role::Owner)], false);
    assert!(
        keys.create(&owner, "default", req("k", Role::Viewer))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn key_role_capped_by_creator() {
    let s = store().await;
    let keys = ApiKeys::new(s);
    let editor = user(&[("default", Role::Editor)], false);
    assert!(matches!(
        keys.create(&editor, "default", req("k", Role::Admin)).await,
        Err(AuthError::Forbidden(_))
    ));
    assert!(matches!(
        keys.create(&editor, "other", req("k", Role::Viewer)).await,
        Err(AuthError::Forbidden(_))
    ));
    assert!(
        keys.create(&editor, "default", req("k", Role::Editor))
            .await
            .is_ok()
    );
    for name in [String::new(), "x".repeat(129)] {
        assert!(matches!(
            keys.create(&editor, "default", req(&name, Role::Viewer))
                .await,
            Err(AuthError::Invalid(_))
        ));
    }
    assert!(
        keys.create(&editor, "default", req(&"x".repeat(128), Role::Viewer))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn sysadmin_key_requires_sysadmin() {
    let s = store().await;
    let keys = ApiKeys::new(s);
    let owner = user(&[("default", Role::Owner)], false);
    let mut r = req("root", Role::Owner);
    r.sysadmin = true;
    assert!(matches!(
        keys.create(&owner, "default", r).await,
        Err(AuthError::Forbidden(_))
    ));
    let mut r = req("root", Role::Owner);
    r.sysadmin = true;
    let (rec, token) = keys
        .create(&Principal::local(), "default", r)
        .await
        .unwrap();
    assert!(rec.sysadmin);
    let k = keys.verify(&token, KeyScope::Admin).await.unwrap();
    let p = ApiKeys::principal(&k);
    assert!(p.sysadmin);
    assert_eq!(p.role_in("elsewhere"), Some(Role::Owner));
}

#[tokio::test]
async fn session_create_lookup_destroy() {
    let s = store().await;
    let sm = SessionManager::new(s.clone(), Duration::from_secs(3600), false);
    let alice = user(&[("default", Role::Editor)], false);
    let cookie = sm.create(&alice).await.unwrap();
    assert_eq!(cookie.len(), 43);
    assert_eq!(sm.lookup(&cookie).await.unwrap(), Some(alice.clone()));
    assert_eq!(sm.lookup("not-a-session").await.unwrap(), None);

    // Authenticator finds it through the cookie header.
    let auth = authenticator(s);
    let mut h = HeaderMap::new();
    h.insert(
        COOKIE,
        format!("theme=dark; {SESSION_COOKIE}={cookie}; x=y")
            .parse()
            .unwrap(),
    );
    assert_eq!(auth.authenticate(&h).await.unwrap(), Some(alice));
    let mut unknown = HeaderMap::new();
    unknown.insert(COOKIE, format!("{SESSION_COOKIE}=nope").parse().unwrap());
    assert_eq!(auth.authenticate(&unknown).await.unwrap(), None);
    assert_eq!(auth.authenticate(&HeaderMap::new()).await.unwrap(), None);

    sm.destroy(&cookie).await.unwrap();
    assert_eq!(sm.lookup(&cookie).await.unwrap(), None);
    assert_eq!(auth.authenticate(&h).await.unwrap(), None);
}

#[tokio::test]
async fn expired_session_is_none_and_deleted() {
    let s = store().await;
    let sm = SessionManager::new(s.clone(), Duration::from_secs(1), false);
    let cookie = sm.create(&Principal::local()).await.unwrap();
    assert!(sm.lookup(&cookie).await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(sm.lookup(&cookie).await.unwrap(), None);
    assert!(
        s.get_admin_session(&sha_hex(&cookie))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn out_of_range_session_ttl_is_an_error_not_a_panic() {
    let s = store().await;
    // Fits chrono's TimeDelta but overflows `now + ttl`.
    let years = Duration::from_secs(1_000_000 * 365 * 24 * 3600);
    for ttl in [years, Duration::MAX] {
        let sm = SessionManager::new(s.clone(), ttl, false);
        assert!(matches!(
            sm.create(&Principal::local()).await,
            Err(AuthError::Invalid(_))
        ));
    }
}

#[tokio::test]
async fn session_ids_are_unique_and_hashed_at_rest() {
    let s = store().await;
    let sm = SessionManager::new(s.clone(), Duration::from_secs(3600), false);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        let c = sm.create(&Principal::local()).await.unwrap();
        assert!(seen.insert(c));
    }
    let c = seen.iter().next().unwrap();
    assert!(s.get_admin_session(c).await.unwrap().is_none());
    let row = s.get_admin_session(&sha_hex(c)).await.unwrap().unwrap();
    assert_eq!(row.principal["subject"], json!("local"));
    assert_eq!(row.principal["kind"], json!("local"));
}

#[tokio::test]
async fn set_cookie_attributes() {
    let s = store().await;
    let plain = SessionManager::new(s.clone(), Duration::from_secs(43200), false);
    assert_eq!(
        plain.set_cookie("abc"),
        "mb_session=abc; HttpOnly; SameSite=Lax; Path=/; Max-Age=43200"
    );
    assert_eq!(
        plain.clear_cookie(),
        "mb_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
    );
    let secure = SessionManager::new(s, Duration::from_secs(43200), true);
    assert_eq!(
        secure.set_cookie("abc"),
        "mb_session=abc; HttpOnly; SameSite=Lax; Path=/; Max-Age=43200; Secure"
    );
    assert_eq!(
        secure.clear_cookie(),
        "mb_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0; Secure"
    );
}

#[tokio::test]
async fn bearer_without_verifier_rejected() {
    let s = store().await;
    let auth = authenticator(s);
    match auth.authenticate(&bearer("eyJhbGciOi.jwt.sig")).await {
        Err(AuthError::Unauthenticated(m)) => assert_eq!(m, "bearer tokens are not accepted"),
        other => panic!("{other:?}"),
    }
    let local = mb_auth::LocalAuth;
    assert_eq!(
        local.authenticate(&HeaderMap::new()).await.unwrap(),
        Some(Principal::local())
    );
}
