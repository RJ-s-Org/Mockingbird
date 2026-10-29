#![cfg(test)]
#![cfg(feature = "mqtt")]

use std::collections::{BTreeSet, HashMap};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use mb_engine::{Engine, EngineConfig};
use mb_ext::EnvFileSecrets;
use mb_mcm::ChannelBinding;
use mb_protocols::events::{
    BrokerClient, BrokerConfig, BrokerError, BrokerKind, Brokers, ChannelRuntime, EventDeps,
    EventSetup, InMessage, MessageStream, OutMessage,
};
use mb_protocols::{MockRuntime, RequestKind, RequestRecord, RequestRecorder};
use mb_state::{MemoryStore, StateStore};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, PublishProperties};
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use tokio::sync::mpsc;

use crate::support::wait_until;

/// The ports [`start_broker`] has tried in this process; none is tried twice.
static TRIED_PORTS: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

/// Start an embedded MQTT 5 broker on a free local port.
///
/// rumqttd binds the port itself, so the port is found by binding port 0
/// and releasing it, and another process (another test) can take it in
/// between. A port that already answers is skipped, and a broker that
/// stops right away (rumqttd logs a failed bind and returns from `start`)
/// means the listener that answers is someone else's: another port is
/// tried, so a test never talks to another test's broker.
fn start_broker() -> u16 {
    for _ in 0..5 {
        if let Some(port) = try_start_broker(untried_port()) {
            return port;
        }
    }
    panic!("the embedded broker could not bind any of 5 ports");
}

/// A free local port this process has not tried before.
fn untried_port() -> u16 {
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if TRIED_PORTS.lock().unwrap().insert(port) {
            return port;
        }
    }
}

/// Start the broker on `port`; `None` when the port turns out to be taken.
fn try_start_broker(port: u16) -> Option<u16> {
    let listen: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    if TcpStream::connect(listen).is_ok() {
        eprintln!("port {port} is in use; trying another");
        return None;
    }
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
    // `start` blocks while the broker serves; it returns only once the
    // listener is gone, for instance because the bind failed.
    let (stopped_tx, stopped) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut broker = rumqttd::Broker::new(config);
        let _ = stopped_tx.send(broker.start().map_err(|e| e.to_string()));
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(listen).is_err() {
        if let Ok(result) = stopped.try_recv() {
            eprintln!("the embedded broker on port {port} stopped: {result:?}");
            return None;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "embedded broker did not start"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Something answers. A broker whose bind failed stops within moments;
    // one that bound keeps serving.
    match stopped.recv_timeout(Duration::from_millis(50)) {
        Ok(result) => {
            eprintln!("port {port} was taken; the broker stopped: {result:?}");
            None
        }
        Err(_) => Some(port),
    }
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

/// Calls `publish` every 100 ms until `stream` yields a message (up to
/// 5 s), and returns that message: for a subscription whose SUBACK the
/// test cannot see.
async fn publish_until_received<F, Fut, E>(stream: &mut MessageStream, mut publish: F) -> InMessage
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: std::fmt::Debug,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        publish().await.unwrap();
        match tokio::time::timeout(Duration::from_millis(100), stream.next()).await {
            Ok(item) => return item.unwrap().unwrap(),
            Err(_) => assert!(
                tokio::time::Instant::now() < deadline,
                "nothing received within 5 s"
            ),
        }
    }
}

/// A plain rumqttc client for the test side.
struct TestClient {
    client: AsyncClient,
    /// Received publishes, as `(topic, payload)`.
    received: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    /// One item per SUBACK.
    subacks: mpsc::UnboundedReceiver<()>,
    _task: tokio::task::JoinHandle<()>,
}

impl TestClient {
    /// Subscribe to `topic` and wait for the broker's SUBACK, after which
    /// the broker routes the topic's publishes to this client.
    async fn subscribe(&mut self, topic: &str) {
        self.client
            .subscribe(topic, QoS::AtLeastOnce)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.subacks.recv())
            .await
            .expect("no SUBACK within 5 s")
            .unwrap();
    }
}

/// A connected [`TestClient`] named `id`.
async fn test_client(port: u16, id: &str) -> TestClient {
    let mut options = MqttOptions::new(id, "127.0.0.1", port);
    options.set_keep_alive(Duration::from_secs(30));
    let (client, mut eventloop) = AsyncClient::new(options, 64);
    let (tx, received) = mpsc::unbounded_channel();
    let (suback_tx, subacks) = mpsc::unbounded_channel();
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
                Ok(Event::Incoming(Packet::SubAck(_))) => {
                    let _ = suback_tx.send(());
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
    TestClient {
        client,
        received,
        subacks,
        _task: task,
    }
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

    let msg = OutMessage {
        key: Some(b"ignored".to_vec()),
        headers: vec![
            ("x-mockingbird-hop".into(), "0".into()),
            ("correlation_id".into(), "c9".into()),
        ],
        payload: br#"{"celsius":21.5}"#.to_vec(),
        content_type: "application/json".into(),
    };
    // The client does not wait for the SUBACK: publish until the
    // subscription is in place and one arrives.
    let target = binding("sensors/s1/temperature");
    let received =
        publish_until_received(&mut stream, || client.publish(&target, msg.clone())).await;
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
    let bundle = mb_import::compile(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/asyncapi/sensors-mqtt-3.0.yaml"
    )))
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

    let mut test = test_client(port, "test-client").await;
    test.subscribe("sensors/s1/acks").await;

    // The runtime's consumer subscribes on its own schedule, unseen by the
    // test: publish the command every 100 ms until one is received.
    let properties = PublishProperties {
        user_properties: vec![("replyTo".into(), "sensors/s1/acks".into())],
        ..PublishProperties::default()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut next_publish = tokio::time::Instant::now();
    let receive = loop {
        if tokio::time::Instant::now() >= next_publish {
            test.client
                .publish_with_properties(
                    "sensors/s1/commands",
                    QoS::AtLeastOnce,
                    false,
                    br#"{"command":"reset"}"#.to_vec(),
                    properties.clone(),
                )
                .await
                .unwrap();
            next_publish += Duration::from_millis(100);
        }
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
            let (topic, payload) = test.received.recv().await.unwrap();
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
    let port = untried_port();
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

#[tokio::test]
async fn broker_lost_after_connect_reaches_issues() {
    let proxy = Proxy::start(start_broker()).await;
    let brokers = Brokers::new(vec![mqtt_config("m", proxy.port)], Arc::new(EnvFileSecrets));
    let client = brokers.client("m").await.unwrap();
    assert!(brokers.issues().is_empty());

    proxy.cut();
    wait_until(
        Duration::from_secs(5),
        Duration::from_millis(10),
        || async {
            if brokers.issues().is_empty() {
                Err("a lost connection must show in issues".to_string())
            } else {
                Ok(())
            }
        },
    )
    .await;
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
    wait_until(
        Duration::from_secs(10),
        Duration::from_millis(10),
        || async {
            let issues = brokers.issues();
            if issues.is_empty() {
                Ok(())
            } else {
                Err(format!("issues: {issues:?}"))
            }
        },
    )
    .await;
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

    let sender = test_client(port, "flooder").await;
    // The broker handles one connection's SUBSCRIBEs in order: once
    // `other` receives, the flood subscription is in place too.
    publish_until_received(&mut other, || {
        sender
            .client
            .publish("other/x", QoS::AtLeastOnce, false, "{}")
    })
    .await;
    for i in 0..500 {
        sender
            .client
            .publish("flood/x", QoS::AtMostOnce, false, format!("{{\"i\":{i}}}"))
            .await
            .unwrap();
    }
    // Kept as a fixed wait: it gives the whole flood time to reach the
    // client, so the bound below is checked against all 500 messages.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut slow = _slow;
    let mut queued = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), slow.next()).await {
        queued += 1;
    }
    assert!(queued > 0 && queued <= 65, "queued {queued}");

    // The event loop keeps delivering to other subscriptions: a message
    // sent after the flood arrives (past any repeats of the probe).
    sender
        .client
        .publish("other/x", QoS::AtLeastOnce, false, r#"{"after":true}"#)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = other.next().await.unwrap().unwrap();
            if msg.payload == br#"{"after":true}"# {
                return;
            }
        }
    })
    .await
    .expect("the message sent after the flood did not arrive");
}

#[tokio::test]
async fn shutdown_flushes_queued_publishes() {
    let port = start_broker();
    let mut watcher = test_client(port, "watcher").await;
    watcher.subscribe("flush/x").await;

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
    while let Ok(Some(_)) =
        tokio::time::timeout(Duration::from_secs(2), watcher.received.recv()).await
    {
        received += 1;
        if received == 50 {
            break;
        }
    }
    assert_eq!(received, 50);
}
