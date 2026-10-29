#![cfg(test)]
#![cfg(feature = "mqtt")]

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use mb_engine::{Engine, EngineConfig};
use mb_ext::EnvFileSecrets;
use mb_mcm::ChannelBinding;
use mb_protocols::events::{
    BrokerClient, BrokerConfig, BrokerError, BrokerKind, Brokers, ChannelRuntime, EventDeps,
    EventSetup, OutMessage,
};
use mb_protocols::{MockRuntime, RequestKind, RequestRecord, RequestRecorder};
use mb_state::{MemoryStore, StateStore};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, PublishProperties};
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use tokio::sync::mpsc;

/// Start an embedded MQTT 5 broker on a free local port.
fn start_broker() -> u16 {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let server = rumqttd::ServerSettings {
        name: "v5-test".into(),
        listen,
        tls: None,
        next_connection_delay_ms: 1,
        connections: rumqttd::ConnectionSettings {
            connection_timeout_ms: 60_000,
            max_payload_size: 2 * 1024 * 1024,
            max_inflight_count: 100,
            auth: None,
            external_auth: None,
            dynamic_filters: true,
        },
    };
    let config = rumqttd::Config {
        id: 0,
        router: rumqttd::RouterConfig {
            max_connections: 100,
            max_outgoing_packet_count: 200,
            max_segment_size: 10 * 1024 * 1024,
            max_segment_count: 10,
            ..Default::default()
        },
        v5: Some(HashMap::from([("1".to_string(), server)])),
        ..Default::default()
    };
    std::thread::spawn(move || {
        let mut broker = rumqttd::Broker::new(config);
        let _ = broker.start();
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(listen).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "embedded broker did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    port
}

fn mqtt_config(name: &str, port: u16) -> BrokerConfig {
    BrokerConfig {
        name: name.into(),
        kind: BrokerKind::Mqtt,
        url: format!("mqtt://127.0.0.1:{port}"),
        tls: false,
        ca_file: None,
        sasl: None,
        client_id_prefix: "mockingbird".into(),
    }
}

fn binding(topic: &str) -> ChannelBinding {
    ChannelBinding::Mqtt {
        topic: topic.into(),
        qos: 1,
        retain: false,
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// A plain rumqttc client for the test side: received publishes arrive
/// on the returned channel as `(topic, payload)`.
async fn test_client(
    port: u16,
    id: &str,
) -> (
    AsyncClient,
    mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    tokio::task::JoinHandle<()>,
) {
    let mut options = MqttOptions::new(id, "127.0.0.1", port);
    options.set_keep_alive(Duration::from_secs(30));
    let (client, mut eventloop) = AsyncClient::new(options, 64);
    let (tx, rx) = mpsc::unbounded_channel();
    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut connected = Some(connected_tx);
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    if let Some(c) = connected.take() {
                        let _ = c.send(());
                    }
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    let topic = String::from_utf8_lossy(&p.topic).into_owned();
                    let _ = tx.send((topic, p.payload.to_vec()));
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(10), connected_rx)
        .await
        .unwrap()
        .unwrap();
    (client, rx, task)
}

#[tokio::test]
async fn mqtt_round_trip_with_headers() {
    let port = start_broker();
    let client = mb_protocols::events::MqttClient::connect(&mqtt_config("m", port), None)
        .await
        .unwrap();
    let mut stream = client
        .subscribe(&binding("sensors/{sensorId}/temperature"), "g")
        .await
        .unwrap();
    // Let the SUBSCRIBE reach the broker.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let msg = OutMessage {
        key: Some(b"ignored".to_vec()),
        headers: vec![
            ("x-mockingbird-hop".into(), "0".into()),
            ("correlation_id".into(), "c9".into()),
        ],
        payload: br#"{"celsius":21.5}"#.to_vec(),
        content_type: "application/json".into(),
    };
    client
        .publish(&binding("sensors/s1/temperature"), msg)
        .await
        .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received.address, "sensors/s1/temperature");
    assert_eq!(received.payload, br#"{"celsius":21.5}"#.to_vec());
    assert_eq!(received.key, None);
    assert_eq!(header(&received.headers, "x-mockingbird-hop"), Some("0"));
    assert_eq!(header(&received.headers, "correlation_id"), Some("c9"));
}

#[derive(Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestRecorder for Capture {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push(r);
    }
}

#[tokio::test]
async fn mqtt_runtime_end_to_end() {
    let port = start_broker();
    let bundle = mb_import::compile(include_bytes!(
        "../../../fixtures/asyncapi/sensors-mqtt-3.0.yaml"
    ))
    .unwrap();
    let records = Arc::new(Capture::default());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "sensors",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        state,
    );
    rt.recorder = Some(Arc::clone(&records) as Arc<dyn RequestRecorder>);
    let brokers = Brokers::new(vec![mqtt_config("mqtt", port)], Arc::new(EnvFileSecrets));
    let deps = EventDeps {
        brokers,
        ..EventDeps::default()
    };
    let channels = ChannelRuntime::start(Arc::new(rt), deps, EventSetup::default());
    assert_eq!(channels.broker_name().as_deref(), Some("mqtt"));

    let (client, mut received, _task) = test_client(port, "test-client").await;
    client
        .subscribe("sensors/s1/acks", QoS::AtLeastOnce)
        .await
        .unwrap();
    // Let the runtime's consumer and the test subscription settle.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let properties = PublishProperties {
        user_properties: vec![("replyTo".into(), "sensors/s1/acks".into())],
        ..PublishProperties::default()
    };
    client
        .publish_with_properties(
            "sensors/s1/commands",
            QoS::AtLeastOnce,
            false,
            br#"{"command":"reset"}"#.to_vec(),
            properties,
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let receive = loop {
        let found = records
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.method == "RECEIVE")
            .cloned();
        if let Some(r) = found {
            break r;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no RECEIVE recorded"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(receive.kind, RequestKind::Event);
    assert_eq!(receive.status, 200);
    assert_eq!(receive.path, "sensors/s1/commands");
    assert_eq!(receive.operation_id.as_deref(), Some("receiveCommand"));

    let (topic, payload) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (topic, payload) = received.recv().await.unwrap();
            if topic == "sensors/s1/acks" {
                return (topic, payload);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(topic, "sensors/s1/acks");
    let ack: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert!(ack.get("command").is_some(), "{ack}");
    assert!(ack.get("ok").is_some(), "{ack}");
}

#[tokio::test]
async fn broker_down_reports_error() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    match mb_protocols::events::MqttClient::connect(&mqtt_config("down", port), None).await {
        Err(BrokerError::Connect(_)) => {}
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("expected a connect error"),
    }

    let brokers = Brokers::new(vec![mqtt_config("down", port)], Arc::new(EnvFileSecrets));
    assert!(brokers.client("down").await.is_err());
    let issues = brokers.issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].0, "down");
}

#[tokio::test]
async fn non_plain_sasl_rejected() {
    let mut cfg = mqtt_config("m", 1);
    cfg.sasl = Some(mb_protocols::events::SaslConfig {
        mechanism: "SCRAM-SHA-256".into(),
        username: "u".into(),
        password_ref: "env:X".into(),
    });
    match mb_protocols::events::MqttClient::connect(&cfg, Some("p".into())).await {
        Err(BrokerError::Connect(msg)) => assert_eq!(msg, "mqtt supports only PLAIN credentials"),
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("expected a connect error"),
    }
}

/// A TCP proxy in front of the broker that the test can cut and restore.
struct Proxy {
    port: u16,
    upstream: u16,
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Proxy {
    async fn start(upstream: u16) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let proxy = Self {
            port,
            upstream,
            tasks: Arc::default(),
        };
        proxy.serve(listener);
        proxy
    }

    fn serve(&self, listener: tokio::net::TcpListener) {
        let upstream = self.upstream;
        let tasks = Arc::clone(&self.tasks);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    return;
                };
                let conn = tokio::spawn(async move {
                    if let Ok(mut outbound) =
                        tokio::net::TcpStream::connect(("127.0.0.1", upstream)).await
                    {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                tasks.lock().unwrap().push(conn);
            }
        });
        self.tasks.lock().unwrap().push(accept);
    }

    /// Close the listener and every proxied connection.
    fn cut(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }

    /// Listen again on the same port.
    async fn restore(&self) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", self.port))
            .await
            .unwrap();
        self.serve(listener);
    }
}

/// Wait up to `timeout` for `cond`.
async fn eventually(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

#[tokio::test]
async fn broker_lost_after_connect_reaches_issues() {
    let proxy = Proxy::start(start_broker()).await;
    let brokers = Brokers::new(vec![mqtt_config("m", proxy.port)], Arc::new(EnvFileSecrets));
    let client = brokers.client("m").await.unwrap();
    assert!(brokers.issues().is_empty());

    proxy.cut();
    assert!(
        eventually(Duration::from_secs(5), || !brokers.issues().is_empty()).await,
        "a lost connection must show in issues"
    );
    assert_eq!(brokers.issues()[0].0, "m");
    assert!(brokers.infos()[0].status.starts_with("error: "));
    // Publishing fails fast while disconnected.
    let msg = OutMessage {
        key: None,
        headers: Vec::new(),
        payload: b"{}".to_vec(),
        content_type: String::new(),
    };
    assert!(matches!(
        client.publish(&binding("a/b"), msg).await,
        Err(BrokerError::Publish(_))
    ));

    proxy.restore().await;
    assert!(
        eventually(Duration::from_secs(10), || brokers.issues().is_empty()).await,
        "issues: {:?}",
        brokers.issues()
    );
}

#[tokio::test]
async fn slow_subscriber_drops_instead_of_queueing() {
    let port = start_broker();
    let client = mb_protocols::events::MqttClient::connect(&mqtt_config("m", port), None)
        .await
        .unwrap();
    // Never read: its queue must stay bounded.
    let _slow = client.subscribe(&binding("flood/x"), "g").await.unwrap();
    let mut other = client.subscribe(&binding("other/x"), "g").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (sender, _rx, _task) = test_client(port, "flooder").await;
    for i in 0..500 {
        sender
            .publish("flood/x", QoS::AtMostOnce, false, format!("{{\"i\":{i}}}"))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut slow = _slow;
    let mut queued = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), slow.next()).await {
        queued += 1;
    }
    assert!(queued > 0 && queued <= 65, "queued {queued}");

    // The event loop keeps delivering to other subscriptions.
    sender
        .publish("other/x", QoS::AtLeastOnce, false, "{}")
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), other.next())
        .await
        .unwrap();
    assert!(matches!(got, Some(Ok(_))));
}

#[tokio::test]
async fn shutdown_flushes_queued_publishes() {
    let port = start_broker();
    let (watcher, mut rx, _task) = test_client(port, "watcher").await;
    watcher
        .subscribe("flush/x", QoS::AtLeastOnce)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let brokers = Brokers::new(vec![mqtt_config("m", port)], Arc::new(EnvFileSecrets));
    let client = brokers.client("m").await.unwrap();
    for i in 0..50 {
        let msg = OutMessage {
            key: None,
            headers: Vec::new(),
            payload: format!("{{\"i\":{i}}}").into_bytes(),
            content_type: String::new(),
        };
        client.publish(&binding("flush/x"), msg).await.unwrap();
    }
    brokers.shutdown(Duration::from_secs(5)).await;
    drop(client);
    drop(brokers);

    let mut received = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        received += 1;
        if received == 50 {
            break;
        }
    }
    assert_eq!(received, 50);
}
