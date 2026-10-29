//! P3 × P4 through a built app: data-plane auth (P3) on the GraphQL and
//! gRPC adapters (P4), and a gRPC mock in a non-default workspace.
#![cfg(test)]

use std::time::Duration;

use crate::support::fixture;
use mb_mcm::ProtocolPayload;
use mb_protocols::grpc::DynamicCodec;
use mockingbird::config::{App, ServeArgs, build_app};
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use serde_json::{Value, json};
use tonic::codegen::http::uri::PathAndQuery;

const WS: &str = "team-a";

async fn serve(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    addr
}

struct Running {
    data: String,
    admin: String,
    grpc: std::net::SocketAddr,
    _app: App,
    _dir: tempfile::TempDir,
}

/// GraphQL and gRPC mocks in `team-a` behind `api_key` data-plane auth.
async fn start() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mockingbird.yaml");
    let config = format!(
        r#"
mocks:
  github:
    workspace: {WS}
    contract: {github}
    data_plane_auth: {{mode: api_key}}
  route-guide:
    workspace: {WS}
    contract: {route_guide}
    data_plane_auth: {{mode: api_key, header: x-mb-key}}
"#,
        github = fixture("graphql/github.graphql").display(),
        route_guide = fixture("proto/route_guide.proto").display(),
    );
    std::fs::write(&path, config).unwrap();
    let args = ServeArgs {
        config: Some(path),
        offline: true,
        ..ServeArgs::default()
    };
    let app = build_app(&args, None).await.unwrap();
    Running {
        data: format!("http://{}", serve(app.data.clone()).await),
        admin: format!("http://{}", serve(app.admin.clone()).await),
        grpc: serve(app.grpc.clone()).await,
        _app: app,
        _dir: dir,
    }
}

/// A data-plane key of workspace `ws`, created through the Admin API (as
/// the local sysadmin of `auth.mode: none`).
async fn data_plane_key(admin: &str, ws: &str) -> String {
    let r = reqwest::Client::new()
        .post(format!("{admin}/api/v1/workspaces/{ws}/api-keys"))
        .json(&json!({"name": "dp", "role": "viewer", "scopes": ["data-plane"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    body["token"].as_str().unwrap().to_string()
}

/// `routeguide.Point` and `routeguide.Feature`.
fn route_guide_messages() -> (MessageDescriptor, MessageDescriptor) {
    let bundle =
        mb_import::compile(&std::fs::read(fixture("proto/route_guide.proto")).unwrap()).unwrap();
    let Some(ProtocolPayload::Grpc {
        file_descriptor_set,
    }) = &bundle.protocol_payload
    else {
        panic!("no descriptors")
    };
    let pool = DescriptorPool::decode(file_descriptor_set.as_slice()).unwrap();
    (
        pool.get_message_by_name("routeguide.Point").unwrap(),
        pool.get_message_by_name("routeguide.Feature").unwrap(),
    )
}

/// One unary `GetFeature` call, with `metadata` as request metadata.
async fn get_feature(
    addr: std::net::SocketAddr,
    metadata: &[(&'static str, &str)],
) -> Result<DynamicMessage, tonic::Status> {
    let (point, feature) = route_guide_messages();
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.unwrap();
    let message =
        DynamicMessage::deserialize(point.clone(), json!({"latitude": 1, "longitude": 2})).unwrap();
    let mut request = tonic::Request::new(message);
    for (name, value) in metadata {
        request.metadata_mut().insert(*name, value.parse().unwrap());
    }
    grpc.unary(
        request,
        PathAndQuery::from_static("/routeguide.RouteGuide/GetFeature"),
        DynamicCodec::new(point, feature),
    )
    .await
    .map(tonic::Response::into_inner)
}

async fn graphql(
    data: &str,
    key: Option<(&str, &str)>,
    query_key: Option<&str>,
) -> reqwest::Response {
    let url = match query_key {
        Some(k) => format!("{data}/github/graphql?api_key={k}"),
        None => format!("{data}/github/graphql"),
    };
    let mut req = reqwest::Client::new()
        .post(url)
        .json(&json!({"query": "{ viewer { login } }"}));
    if let Some((name, value)) = key {
        req = req.header(name, value);
    }
    req.send().await.unwrap()
}

/// (1) GraphQL and gRPC mocks with `data_plane_auth: api_key` refuse calls
/// without a valid key of their workspace and serve calls with one.
#[tokio::test]
async fn graphql_and_grpc_enforce_data_plane_keys() {
    let app = start().await;
    let key = data_plane_key(&app.admin, WS).await;
    let other = data_plane_key(&app.admin, "default").await;

    // GraphQL: no key, a wrong key, another workspace's key → 401.
    let r = graphql(&app.data, None, None).await;
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.headers()["www-authenticate"],
        r#"ApiKey realm="mockingbird", header="x-api-key""#
    );
    assert_eq!(
        graphql(&app.data, Some(("x-api-key", "mbk_nope_nope")), None)
            .await
            .status(),
        401
    );
    assert_eq!(
        graphql(&app.data, Some(("x-api-key", &other)), None)
            .await
            .status(),
        401
    );
    // A valid key in the header or the `api_key` parameter → served.
    for r in [
        graphql(&app.data, Some(("x-api-key", &key)), None).await,
        graphql(&app.data, None, Some(&key)).await,
    ] {
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        assert!(body["data"]["viewer"]["login"].is_string(), "{body}");
        assert!(body.get("errors").is_none(), "{body}");
    }

    // gRPC: the key travels in the request metadata (header `x-mb-key`).
    let err = get_feature(app.grpc, &[]).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");
    let err = get_feature(app.grpc, &[("x-mb-key", &other)])
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");
    // The default header name is not this mock's.
    let err = get_feature(app.grpc, &[("x-api-key", &key)])
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "{err:?}");
    let reply = get_feature(app.grpc, &[("x-mb-key", &key)]).await.unwrap();
    assert_eq!(
        prost_reflect::ReflectMessage::descriptor(&reply).full_name(),
        "routeguide.Feature"
    );
}

/// (2) A gRPC mock in a non-default workspace is listed and records its
/// calls under that workspace, never under a same-named mock elsewhere;
/// the data-plane key is never recorded.
#[tokio::test]
async fn grpc_mock_in_workspace_records_there() {
    let app = start().await;
    let http = reqwest::Client::new();
    let key = data_plane_key(&app.admin, WS).await;

    let listed: Value = http
        .get(format!("{}/api/v1/workspaces/{WS}/mocks", app.admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"route-guide"), "{listed}");
    let rg = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "route-guide")
        .unwrap();
    assert_eq!(rg["workspace"], WS);
    assert_eq!(rg["data_plane_auth"]["mode"], "api_key");
    let default: Value = http
        .get(format!("{}/api/v1/workspaces/default/mocks", app.admin))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !default
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "route-guide"),
        "{default}"
    );

    // A same-named mock in `default` (mock names are per workspace).
    let r = http
        .post(format!("{}/api/v1/workspaces/default/mocks", app.admin))
        .json(&json!({"name": "route-guide", "route_prefix": "/rg-default"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    get_feature(app.grpc, &[]).await.unwrap_err();
    get_feature(app.grpc, &[("x-mb-key", &key), ("x-trace", "t1")])
        .await
        .unwrap();

    // The request log is written in the background.
    let url = format!(
        "{}/api/v1/workspaces/{WS}/mocks/route-guide/requests",
        app.admin
    );
    let mut entries = Value::Null;
    for _ in 0..100 {
        entries = http.get(&url).send().await.unwrap().json().await.unwrap();
        if entries.as_array().is_some_and(|e| e.len() == 2) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let entries = entries.as_array().unwrap_or_else(|| panic!("{entries}"));
    assert_eq!(entries.len(), 2, "{entries:?}");
    let mut statuses: Vec<u64> = entries
        .iter()
        .map(|e| e["status"].as_u64().unwrap())
        .collect();
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 401]);
    for e in entries {
        assert_eq!(e["path"], "/routeguide.RouteGuide/GetFeature", "{e}");
        let headers = &e["request"]["headers"];
        assert!(headers.get("x-mb-key").is_none(), "key recorded: {e}");
        assert!(!e.to_string().contains(&key), "key recorded: {e}");
    }
    assert!(
        entries
            .iter()
            .any(|e| e["request"]["headers"]["x-trace"] == "t1"),
        "{entries:?}"
    );

    // Nothing was attributed to the same-named mock of `default`.
    let r = http
        .get(format!(
            "{}/api/v1/workspaces/default/mocks/route-guide/requests",
            app.admin
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap(), json!([]));
}
