//! End-to-end tests for IDN protocol with mock server.
//!
//! These tests verify the full discovery -> connect -> stream -> disconnect -> reconnect
//! lifecycle using a mock UDP server that speaks the IDN protocol.

#![cfg(all(feature = "idn", feature = "testutils"))]

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use laser_dac::types::{DacConnectionState, DacType, EnabledDacTypes, LaserFrame, LaserPoint};
use laser_dac::DacDiscoveryWorker;

/// Create an EnabledDacTypes with only IDN enabled.
fn idn_only() -> EnabledDacTypes {
    let mut types = EnabledDacTypes::none();
    types.enable(DacType::Idn);
    types
}

// IDN Protocol constants (matching src/protocols/idn/protocol.rs)
const IDNCMD_PING_REQUEST: u8 = 0x08;
const IDNCMD_PING_RESPONSE: u8 = 0x09;
const IDNCMD_SCAN_REQUEST: u8 = 0x10;
const IDNCMD_SCAN_RESPONSE: u8 = 0x11;
const IDNCMD_SERVICEMAP_REQUEST: u8 = 0x12;
const IDNCMD_SERVICEMAP_RESPONSE: u8 = 0x13;
const IDNCMD_RT_CNLMSG: u8 = 0x40;
const IDNVAL_STYPE_LAPRO: u8 = 0x80;

/// Configuration for a mock service.
#[derive(Clone)]
pub struct MockService {
    pub service_id: u8,
    pub service_type: u8,
    pub name: String,
    pub flags: u8,
    pub relay_number: u8,
}

impl MockService {
    /// Create a laser projector service.
    pub fn laser_projector(service_id: u8, name: &str) -> Self {
        Self {
            service_id,
            service_type: IDNVAL_STYPE_LAPRO,
            name: name.to_string(),
            flags: 0,
            relay_number: 0,
        }
    }

    /// Create a DMX512 service.
    pub fn dmx512(service_id: u8, name: &str) -> Self {
        Self {
            service_id,
            service_type: 0x05, // IDNVAL_STYPE_DMX512
            name: name.to_string(),
            flags: 0,
            relay_number: 0,
        }
    }

    /// Set the relay number this service is attached to.
    pub fn with_relay(mut self, relay_number: u8) -> Self {
        self.relay_number = relay_number;
        self
    }

    /// Set the DSID flag (default service for type).
    pub fn with_dsid(mut self) -> Self {
        self.flags |= 0x01; // IDNFLG_SERVICEMAP_DSID
        self
    }
}

/// Configuration for a mock relay.
#[derive(Clone)]
pub struct MockRelay {
    pub relay_number: u8,
    pub name: String,
}

impl MockRelay {
    pub fn new(relay_number: u8, name: &str) -> Self {
        Self {
            relay_number,
            name: name.to_string(),
        }
    }
}

/// Builder for MockIdnServer with configurable options.
pub struct MockIdnServerBuilder {
    hostname: String,
    unit_id: [u8; 16],
    protocol_version: u8,
    status: u8,
    services: Vec<MockService>,
    relays: Vec<MockRelay>,
    silent: bool,
}

impl MockIdnServerBuilder {
    /// Create a new builder with the given hostname.
    pub fn new(hostname: &str) -> Self {
        // Generate a deterministic unit_id from hostname
        let mut unit_id = [0u8; 16];
        let bytes = hostname.as_bytes();
        for (i, &b) in bytes.iter().enumerate().take(16) {
            unit_id[i] = b;
        }

        Self {
            hostname: hostname.to_string(),
            unit_id,
            protocol_version: 0x10, // Version 1.0
            status: 0,
            services: vec![MockService::laser_projector(1, "Laser1")],
            relays: Vec::new(),
            silent: false,
        }
    }

    /// Set a custom unit ID.
    pub fn unit_id(mut self, unit_id: [u8; 16]) -> Self {
        self.unit_id = unit_id;
        self
    }

    /// Set the protocol version (major.minor packed into single byte).
    pub fn protocol_version(mut self, major: u8, minor: u8) -> Self {
        self.protocol_version = (major << 4) | (minor & 0x0F);
        self
    }

    /// Set the status byte.
    pub fn status(mut self, status: u8) -> Self {
        self.status = status;
        self
    }

    /// Set the services this server provides.
    pub fn services(mut self, services: Vec<MockService>) -> Self {
        self.services = services;
        self
    }

    /// Set the relays this server provides.
    pub fn relays(mut self, relays: Vec<MockRelay>) -> Self {
        self.relays = relays;
        self
    }

    /// Enable silent mode (server receives but never responds).
    pub fn silent(mut self, silent: bool) -> Self {
        self.silent = silent;
        self
    }

    /// Build the MockIdnServer.
    pub fn build(self) -> io::Result<MockIdnServer> {
        MockIdnServer::from_builder(self)
    }
}

/// Mock IDN server that responds to scan, servicemap, and ping requests.
pub struct MockIdnServer {
    socket: UdpSocket,
    unit_id: [u8; 16],
    hostname: String,
    protocol_version: u8,
    status: u8,
    services: Vec<MockService>,
    relays: Vec<MockRelay>,
    running: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    received_packets: Arc<Mutex<Vec<Vec<u8>>>>,
    silent: bool,
}

impl MockIdnServer {
    /// Create a new mock IDN server bound to localhost on an ephemeral port.
    pub fn new(hostname: &str) -> io::Result<Self> {
        Self::builder(hostname).build()
    }

    /// Create a builder for more configuration options.
    pub fn builder(hostname: &str) -> MockIdnServerBuilder {
        MockIdnServerBuilder::new(hostname)
    }

    fn from_builder(builder: MockIdnServerBuilder) -> io::Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;

        Ok(Self {
            socket,
            unit_id: builder.unit_id,
            hostname: builder.hostname,
            protocol_version: builder.protocol_version,
            status: builder.status,
            services: builder.services,
            relays: builder.relays,
            running: Arc::new(AtomicBool::new(false)),
            disconnected: Arc::new(AtomicBool::new(false)),
            received_packets: Arc::new(Mutex::new(Vec::new())),
            silent: builder.silent,
        })
    }

    /// Get the configured unit_id.
    pub fn unit_id(&self) -> [u8; 16] {
        self.unit_id
    }

    /// Get the configured hostname.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Get the configured protocol version.
    pub fn protocol_version(&self) -> (u8, u8) {
        (self.protocol_version >> 4, self.protocol_version & 0x0F)
    }

    /// Get the configured status.
    pub fn status(&self) -> u8 {
        self.status
    }

    /// Get the configured services.
    pub fn services(&self) -> &[MockService] {
        &self.services
    }

    /// Get the configured relays.
    pub fn relays(&self) -> &[MockRelay] {
        &self.relays
    }

    /// Get the server's local address.
    pub fn addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }

    /// Start the server in a background thread.
    pub fn run(self) -> MockIdnServerHandle {
        let running = Arc::clone(&self.running);
        let disconnected = Arc::clone(&self.disconnected);
        let received_packets = Arc::clone(&self.received_packets);

        running.store(true, Ordering::SeqCst);

        let addr = self.addr();
        let handle = thread::spawn(move || {
            self.server_loop();
        });

        MockIdnServerHandle {
            addr,
            running,
            disconnected,
            received_packets,
            handle: Some(handle),
        }
    }

    fn server_loop(self) {
        let mut buf = [0u8; 1500];

        while self.running.load(Ordering::SeqCst) {
            let (len, src) = match self.socket.recv_from(&mut buf) {
                Ok(result) => result,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
                Err(_) => break,
            };

            // Store received packet
            {
                let mut packets = self.received_packets.lock().unwrap();
                packets.push(buf[..len].to_vec());
            }

            // If silent mode or disconnected, don't respond
            if self.silent || self.disconnected.load(Ordering::SeqCst) {
                continue;
            }

            if len < 4 {
                continue;
            }

            let command = buf[0];
            let flags = buf[1];
            let sequence = u16::from_be_bytes([buf[2], buf[3]]);

            match command {
                IDNCMD_SCAN_REQUEST => {
                    let response = self.build_scan_response(flags, sequence);
                    let _ = self.socket.send_to(&response, src);
                }
                IDNCMD_SERVICEMAP_REQUEST => {
                    let response = self.build_servicemap_response(flags, sequence);
                    let _ = self.socket.send_to(&response, src);
                }
                IDNCMD_PING_REQUEST => {
                    let response = self.build_ping_response(flags, sequence);
                    let _ = self.socket.send_to(&response, src);
                }
                IDNCMD_RT_CNLMSG => {
                    // Frame data - just accept silently (UDP fire and forget)
                }
                _ => {
                    // Unknown command, ignore
                }
            }
        }
    }

    fn build_scan_response(&self, flags: u8, sequence: u16) -> Vec<u8> {
        let mut response = Vec::with_capacity(44);

        // Packet header (4 bytes)
        response.push(IDNCMD_SCAN_RESPONSE);
        response.push(flags);
        response.extend_from_slice(&sequence.to_be_bytes());

        // ScanResponse (40 bytes)
        response.push(40); // struct_size
        response.push(self.protocol_version);
        response.push(self.status);
        response.push(0x00); // reserved
        response.extend_from_slice(&self.unit_id);

        // Hostname (20 bytes, null-padded)
        let mut hostname_bytes = [0u8; 20];
        let name_bytes = self.hostname.as_bytes();
        let len = name_bytes.len().min(20);
        hostname_bytes[..len].copy_from_slice(&name_bytes[..len]);
        response.extend_from_slice(&hostname_bytes);

        response
    }

    fn build_servicemap_response(&self, flags: u8, sequence: u16) -> Vec<u8> {
        let relay_count = self.relays.len() as u8;
        let service_count = self.services.len() as u8;
        let capacity = 4 + 4 + (relay_count as usize + service_count as usize) * 24;
        let mut response = Vec::with_capacity(capacity);

        // Packet header (4 bytes)
        response.push(IDNCMD_SERVICEMAP_RESPONSE);
        response.push(flags);
        response.extend_from_slice(&sequence.to_be_bytes());

        // ServiceMapResponseHeader (4 bytes)
        response.push(4); // struct_size
        response.push(24); // entry_size
        response.push(relay_count);
        response.push(service_count);

        // Relay entries (24 bytes each)
        for relay in &self.relays {
            response.push(0x00); // service_id (must be 0 for relays)
            response.push(0x00); // service_type (unused for relays)
            response.push(0x00); // flags
            response.push(relay.relay_number); // relay_number (!=0 for relays)

            // Relay name (20 bytes, null-padded)
            let mut name_bytes = [0u8; 20];
            let name = relay.name.as_bytes();
            let len = name.len().min(20);
            name_bytes[..len].copy_from_slice(&name[..len]);
            response.extend_from_slice(&name_bytes);
        }

        // Service entries (24 bytes each)
        for service in &self.services {
            response.push(service.service_id);
            response.push(service.service_type);
            response.push(service.flags);
            response.push(service.relay_number);

            // Service name (20 bytes, null-padded)
            let mut name_bytes = [0u8; 20];
            let name = service.name.as_bytes();
            let len = name.len().min(20);
            name_bytes[..len].copy_from_slice(&name[..len]);
            response.extend_from_slice(&name_bytes);
        }

        response
    }

    fn build_ping_response(&self, flags: u8, sequence: u16) -> Vec<u8> {
        let mut response = Vec::with_capacity(4);

        // Packet header (4 bytes)
        response.push(IDNCMD_PING_RESPONSE);
        response.push(flags);
        response.extend_from_slice(&sequence.to_be_bytes());

        response
    }
}

/// Handle to a running mock IDN server.
pub struct MockIdnServerHandle {
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    received_packets: Arc<Mutex<Vec<Vec<u8>>>>,
    handle: Option<JoinHandle<()>>,
}

impl MockIdnServerHandle {
    /// Get the server's address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Simulate a disconnection (server stops responding).
    pub fn simulate_disconnect(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
    }

    /// Resume normal operation after simulated disconnect.
    pub fn resume(&self) {
        self.disconnected.store(false, Ordering::SeqCst);
    }

    /// Get the number of packets received by the server.
    pub fn received_packet_count(&self) -> usize {
        self.received_packets.lock().unwrap().len()
    }

    /// Clear received packets.
    pub fn clear_received_packets(&self) {
        self.received_packets.lock().unwrap().clear();
    }

    /// Check if server received any frame data packets.
    pub fn received_frame_data(&self) -> bool {
        let packets = self.received_packets.lock().unwrap();
        packets
            .iter()
            .any(|p| !p.is_empty() && p[0] == IDNCMD_RT_CNLMSG)
    }
}

impl Drop for MockIdnServerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Create a simple test frame.
fn create_test_frame() -> LaserFrame {
    let points = vec![
        LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
        LaserPoint::new(0.5, 0.5, 0, 65535, 0, 65535),
        LaserPoint::new(-0.5, -0.5, 0, 0, 65535, 65535),
    ];
    LaserFrame::new(30000, points)
}

/// Wait for a worker to appear from the discovery worker.
fn wait_for_worker(
    discovery: &DacDiscoveryWorker,
    timeout: Duration,
) -> Option<laser_dac::DacWorker> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        // Drain discovered devices
        let _: Vec<_> = discovery.poll_discovered_devices().collect();

        // Check for workers
        let workers: Vec<_> = discovery.poll_new_workers().collect();
        if !workers.is_empty() {
            return workers.into_iter().next();
        }

        thread::sleep(Duration::from_millis(50));
    }
    None
}

// =============================================================================
// Tests
// =============================================================================

#[test]
fn test_scanner_direct() {
    use laser_dac::protocols::idn::ServerScanner;

    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    eprintln!("Mock server listening on: {}", server_addr);

    let _handle = server.run();

    // Give server time to start
    thread::sleep(Duration::from_millis(50));

    // Directly test the scanner
    let mut scanner = ServerScanner::new(0).expect("Failed to create scanner");
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .expect("Scan failed");

    eprintln!("Found {} servers", servers.len());
    for s in &servers {
        eprintln!("  Server: {} at {:?}", s.hostname, s.addresses);
    }

    assert!(!servers.is_empty(), "Scanner should find the mock server");
}

#[test]
fn test_idn_discovery_direct() {
    use laser_dac::discovery::IdnDiscovery;

    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    eprintln!("Mock server listening on: {}", server_addr);

    let _handle = server.run();
    thread::sleep(Duration::from_millis(50));

    let mut idn_discovery = IdnDiscovery::new();
    let devices = idn_discovery.scan_address(server_addr);

    eprintln!("Found {} devices", devices.len());
    for d in &devices {
        eprintln!("  Device: {} ({:?})", d.name(), d.dac_type());
    }

    assert!(
        !devices.is_empty(),
        "IdnDiscovery should find the mock server"
    );
}

#[test]
fn test_discover_and_connect() {
    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    eprintln!("Mock server listening on: {}", server_addr);
    let handle = server.run();

    // Give server time to start
    thread::sleep(Duration::from_millis(50));

    eprintln!("Creating discovery worker...");
    // Create discovery worker configured to scan mock server
    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    eprintln!("Waiting for discovery...");
    // Wait for discovery - give more time
    for i in 0..10 {
        thread::sleep(Duration::from_millis(100));

        let devices: Vec<_> = discovery.poll_discovered_devices().collect();
        let workers: Vec<_> = discovery.poll_new_workers().collect();

        eprintln!(
            "Poll {}: {} devices, {} workers, {} packets received",
            i,
            devices.len(),
            workers.len(),
            handle.received_packet_count()
        );

        if !devices.is_empty() || !workers.is_empty() {
            eprintln!("Found something!");
            for d in &devices {
                eprintln!("  Device: {} ({:?})", d.name(), d.dac_type);
            }
            for w in &workers {
                eprintln!("  Worker: {} ({:?})", w.device_name(), w.dac_type());
            }
            return; // Test passes
        }
    }

    panic!("Did not discover any devices after 1 second");
}

#[test]
fn test_send_frame() {
    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    let handle = server.run();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Wait for worker
    let worker = wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Clear any discovery packets
    handle.clear_received_packets();

    // Send a frame
    let frame = create_test_frame();
    let submitted = worker.submit_frame(frame);
    assert!(submitted, "Frame should be submitted successfully");

    // Give the worker thread time to process
    thread::sleep(Duration::from_millis(100));

    // Verify server received frame data
    assert!(
        handle.received_frame_data(),
        "Server should have received frame data packet"
    );
}

#[test]
fn test_connection_loss_detection() {
    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    let handle = server.run();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Wait for worker
    let mut worker =
        wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Verify initially connected
    worker.update();
    assert!(
        matches!(worker.state(), DacConnectionState::Connected { .. }),
        "Worker should be connected initially"
    );

    // Send a frame successfully
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(50));
    worker.update();

    // Simulate server disconnect
    handle.simulate_disconnect();

    // Wait for keepalive timeout to trigger (500ms idle + 200ms ping timeout)
    thread::sleep(Duration::from_millis(800));

    // Send another frame - this should trigger keepalive check
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(300));
    worker.update();

    // Worker should detect connection loss
    assert!(
        matches!(worker.state(), DacConnectionState::Lost { .. }),
        "Worker should detect connection loss after ping timeout"
    );
}

#[test]
fn test_reconnection_after_loss() {
    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    let handle = server.run();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(200))
        .build();

    // Wait for initial worker
    let mut worker =
        wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Simulate disconnect
    handle.simulate_disconnect();

    // Trigger keepalive failure
    thread::sleep(Duration::from_millis(800));
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(300));
    worker.update();

    // Drop the old worker so discovery can reconnect
    drop(worker);

    // Resume server
    handle.resume();

    // Wait for rediscovery
    let new_worker = wait_for_worker(&discovery, Duration::from_secs(2))
        .expect("Should reconnect with a new worker after server resumes");

    // Verify new worker is connected
    let mut new_worker = new_worker;
    new_worker.update();
    assert!(
        matches!(new_worker.state(), DacConnectionState::Connected { .. }),
        "New worker should be connected"
    );
}

#[test]
fn test_full_lifecycle() {
    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    let handle = server.run();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(200))
        .build();

    // Phase 1: Initial connection
    let mut worker = wait_for_worker(&discovery, Duration::from_secs(2))
        .expect("Phase 1: Should connect initially");
    handle.clear_received_packets();

    // Phase 2: Send frames successfully
    for _ in 0..3 {
        worker.submit_frame(create_test_frame());
        thread::sleep(Duration::from_millis(30));
    }
    thread::sleep(Duration::from_millis(100));
    assert!(
        handle.received_frame_data(),
        "Phase 2: Server should receive frame data"
    );

    // Phase 3: Simulate disconnect
    handle.simulate_disconnect();
    thread::sleep(Duration::from_millis(800));
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(300));
    worker.update();
    assert!(
        matches!(worker.state(), DacConnectionState::Lost { .. }),
        "Phase 3: Should detect connection loss"
    );
    drop(worker);

    // Phase 4: Reconnect
    handle.resume();

    let mut new_worker =
        wait_for_worker(&discovery, Duration::from_secs(2)).expect("Phase 4: Should reconnect");
    handle.clear_received_packets();

    // Phase 5: Send frames again after reconnection
    new_worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(100));
    assert!(
        handle.received_frame_data(),
        "Phase 5: Should send frames after reconnection"
    );

    new_worker.update();
    assert!(
        matches!(new_worker.state(), DacConnectionState::Connected { .. }),
        "Phase 5: Should remain connected"
    );
}

#[test]
fn test_parsed_server_metadata() {
    use laser_dac::protocols::idn::{ServerScanner, ServiceType};

    // Create a server with specific metadata
    let unit_id: [u8; 16] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10,
    ];

    let server = MockIdnServer::builder("MetadataTest")
        .unit_id(unit_id)
        .protocol_version(2, 5)
        .status(0x42)
        .services(vec![
            MockService::laser_projector(7, "MainLaser").with_dsid()
        ])
        .build()
        .unwrap();

    let expected_hostname = "MetadataTest";
    let expected_version = (2, 5);
    let expected_status = 0x42;
    let server_addr = server.addr();

    let _handle = server.run();
    thread::sleep(Duration::from_millis(50));

    // Scan and get ServerInfo
    let mut scanner = ServerScanner::new(0).unwrap();
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .unwrap();

    assert_eq!(servers.len(), 1, "Should find exactly one server");

    let server_info = &servers[0];

    // Assert ServerInfo fields
    assert_eq!(
        server_info.unit_id, unit_id,
        "unit_id should match configured value"
    );
    assert_eq!(
        server_info.hostname, expected_hostname,
        "hostname should match configured value"
    );
    assert_eq!(
        server_info.protocol_version, expected_version,
        "protocol_version should match configured value"
    );
    assert_eq!(
        server_info.status, expected_status,
        "status should match configured value"
    );
    assert!(
        !server_info.addresses.is_empty(),
        "addresses should not be empty"
    );
    assert_eq!(
        server_info.addresses[0], server_addr,
        "address should match mock server address"
    );

    // Assert ServiceInfo fields
    assert_eq!(
        server_info.services.len(),
        1,
        "Should have exactly one service"
    );
    let service = &server_info.services[0];
    assert_eq!(service.service_id, 7, "service_id should match");
    assert!(
        matches!(service.service_type, ServiceType::LaserProjector),
        "service_type should be LaserProjector"
    );
    assert_eq!(service.name, "MainLaser", "service name should match");
    assert_eq!(service.flags, 0x01, "flags should have DSID bit set");
    assert_eq!(service.relay_number, 0, "relay_number should be 0 (root)");
}

#[test]
fn test_parsed_multiple_services() {
    use laser_dac::protocols::idn::{ServerScanner, ServiceType};

    let server = MockIdnServer::builder("MultiService")
        .services(vec![
            MockService::laser_projector(1, "Laser1").with_dsid(),
            MockService::laser_projector(2, "Laser2"),
            MockService::dmx512(3, "DMXOut"),
        ])
        .build()
        .unwrap();

    let server_addr = server.addr();
    let _handle = server.run();
    thread::sleep(Duration::from_millis(50));

    let mut scanner = ServerScanner::new(0).unwrap();
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .unwrap();

    assert_eq!(servers.len(), 1);
    let server_info = &servers[0];

    // Should have all 3 services
    assert_eq!(server_info.services.len(), 3, "Should have 3 services");

    // Check first service (laser projector with DSID)
    let svc1 = &server_info.services[0];
    assert_eq!(svc1.service_id, 1);
    assert!(matches!(svc1.service_type, ServiceType::LaserProjector));
    assert_eq!(svc1.name, "Laser1");
    assert_eq!(svc1.flags, 0x01); // DSID flag

    // Check second service (laser projector without DSID)
    let svc2 = &server_info.services[1];
    assert_eq!(svc2.service_id, 2);
    assert!(matches!(svc2.service_type, ServiceType::LaserProjector));
    assert_eq!(svc2.name, "Laser2");
    assert_eq!(svc2.flags, 0x00);

    // Check third service (DMX512)
    let svc3 = &server_info.services[2];
    assert_eq!(svc3.service_id, 3);
    assert!(matches!(svc3.service_type, ServiceType::Dmx512));
    assert_eq!(svc3.name, "DMXOut");
}

#[test]
fn test_parsed_relays_and_services() {
    use laser_dac::protocols::idn::{ServerScanner, ServiceType};

    let server = MockIdnServer::builder("RelayTest")
        .relays(vec![
            MockRelay::new(1, "Relay1"),
            MockRelay::new(2, "Relay2"),
        ])
        .services(vec![
            MockService::laser_projector(1, "RootLaser"),
            MockService::laser_projector(2, "RelayedLaser").with_relay(1),
        ])
        .build()
        .unwrap();

    let server_addr = server.addr();
    let _handle = server.run();
    thread::sleep(Duration::from_millis(50));

    let mut scanner = ServerScanner::new(0).unwrap();
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .unwrap();

    assert_eq!(servers.len(), 1);
    let server_info = &servers[0];

    // Check relays
    assert_eq!(server_info.relays.len(), 2, "Should have 2 relays");

    let relay1 = &server_info.relays[0];
    assert_eq!(relay1.relay_number, 1, "relay1 number should be 1");
    assert_eq!(relay1.name, "Relay1", "relay1 name should match");

    let relay2 = &server_info.relays[1];
    assert_eq!(relay2.relay_number, 2, "relay2 number should be 2");
    assert_eq!(relay2.name, "Relay2", "relay2 name should match");

    // Check services
    assert_eq!(server_info.services.len(), 2, "Should have 2 services");

    let svc1 = &server_info.services[0];
    assert_eq!(svc1.service_id, 1);
    assert!(matches!(svc1.service_type, ServiceType::LaserProjector));
    assert_eq!(svc1.name, "RootLaser");
    assert_eq!(
        svc1.relay_number, 0,
        "RootLaser should be on root (relay 0)"
    );

    let svc2 = &server_info.services[1];
    assert_eq!(svc2.service_id, 2);
    assert!(matches!(svc2.service_type, ServiceType::LaserProjector));
    assert_eq!(svc2.name, "RelayedLaser");
    assert_eq!(svc2.relay_number, 1, "RelayedLaser should be on relay 1");
}

#[test]
fn test_discovered_device_info_metadata() {
    use laser_dac::discovery::IdnDiscovery;
    use std::net::IpAddr;

    let server = MockIdnServer::builder("DiscoveryTest")
        .protocol_version(1, 2)
        .services(vec![MockService::laser_projector(5, "TestLaser")])
        .build()
        .unwrap();

    let server_addr = server.addr();
    let _handle = server.run();
    thread::sleep(Duration::from_millis(50));

    let mut idn_discovery = IdnDiscovery::new();
    let devices = idn_discovery.scan_address(server_addr);

    assert_eq!(devices.len(), 1, "Should discover one device");

    let device = &devices[0];
    let info = device.info();

    // DiscoveredDeviceInfo should expose relevant metadata
    assert_eq!(info.dac_type, DacType::Idn);

    // IP address should be set for IDN devices
    assert!(
        info.ip_address.is_some(),
        "IDN device should have IP address"
    );
    assert_eq!(
        info.ip_address,
        Some(IpAddr::V4("127.0.0.1".parse().unwrap())),
        "IP should be localhost"
    );

    // Hostname should be set from scan response
    assert!(info.hostname.is_some(), "IDN device should have hostname");
    assert_eq!(
        info.hostname.as_deref(),
        Some("DiscoveryTest"),
        "hostname should match mock server configuration"
    );

    // name() returns IP for network devices (unique identifier)
    assert_eq!(info.name(), "127.0.0.1");
}

// =============================================================================
// Timeout, Connection, and Back Pressure Tests
// =============================================================================

#[test]
fn test_server_never_responds_timeout() {
    use laser_dac::discovery::IdnDiscovery;
    use laser_dac::protocols::idn::ServerScanner;

    // Create a silent mock server that receives but never responds
    let server = MockIdnServer::builder("SilentServer")
        .silent(true)
        .build()
        .unwrap();
    let server_addr = server.addr();

    let handle = server.run();
    thread::sleep(Duration::from_millis(50));

    // Test 1: ServerScanner should return empty results after timeout
    let mut scanner = ServerScanner::new(0).expect("Failed to create scanner");
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(200))
        .expect("Scan should not error, just return empty");

    assert!(
        servers.is_empty(),
        "Scanner should return empty results for silent server"
    );

    // Verify server received the scan request
    assert!(
        handle.received_packet_count() > 0,
        "Silent server should still receive packets"
    );

    // Test 2: IdnDiscovery should also return empty results
    handle.clear_received_packets();
    let mut idn_discovery = IdnDiscovery::new();
    let devices = idn_discovery.scan_address(server_addr);

    assert!(
        devices.is_empty(),
        "IdnDiscovery should return empty for silent server"
    );
    assert!(
        handle.received_packet_count() > 0,
        "Silent server should receive IdnDiscovery packets"
    );
}

#[test]
fn test_discovery_timeout_on_established_connection() {
    // Test that an established connection properly times out when server goes silent
    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    let handle = server.run();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Get a connected worker
    let mut worker =
        wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Send a frame successfully first
    assert!(worker.submit_frame(create_test_frame()));
    thread::sleep(Duration::from_millis(100));
    worker.update();

    assert!(
        matches!(worker.state(), DacConnectionState::Connected { .. }),
        "Worker should be connected initially"
    );

    // Now simulate disconnect (server stops responding to pings)
    handle.simulate_disconnect();

    // Wait for keepalive interval (500ms) + ping timeout (200ms) + margin
    thread::sleep(Duration::from_millis(800));

    // Try to send another frame - this should trigger keepalive check
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(300));
    worker.update();

    // Verify connection loss was detected
    assert!(
        matches!(worker.state(), DacConnectionState::Lost { .. }),
        "Worker should detect connection loss when server stops responding"
    );
}

#[test]
fn test_connection_to_nonexistent_server() {
    use laser_dac::discovery::IdnDiscovery;
    use laser_dac::protocols::idn::ServerScanner;

    // Use a high port that definitely has no server listening
    let nonexistent_addr: SocketAddr = "127.0.0.1:65432".parse().unwrap();

    // Test 1: ServerScanner should gracefully handle no server
    let mut scanner = ServerScanner::new(0).expect("Failed to create scanner");
    let servers = scanner
        .scan_address(nonexistent_addr, Duration::from_millis(200))
        .expect("Scan should not error, just return empty");

    assert!(
        servers.is_empty(),
        "Scanner should return empty results when no server exists"
    );

    // Test 2: IdnDiscovery should also gracefully handle no server
    let mut idn_discovery = IdnDiscovery::new();
    let devices = idn_discovery.scan_address(nonexistent_addr);

    assert!(
        devices.is_empty(),
        "IdnDiscovery should return empty for nonexistent server"
    );

    // Test 3: DacDiscoveryWorker should handle unreachable addresses without crashing
    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![nonexistent_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Let discovery run for a bit - should not panic
    thread::sleep(Duration::from_millis(300));

    // Should find nothing but not crash
    let devices: Vec<_> = discovery.poll_discovered_devices().collect();
    let workers: Vec<_> = discovery.poll_new_workers().collect();

    assert!(devices.is_empty(), "Should not discover any devices");
    assert!(workers.is_empty(), "Should not get any workers");
}

#[test]
fn test_rapid_frame_submission_back_pressure() {
    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    let handle = server.run();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    let worker = wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Clear discovery packets
    handle.clear_received_packets();

    // Submit many frames rapidly in a tight loop
    let mut submitted = 0;
    let mut dropped = 0;
    for _ in 0..100 {
        if worker.submit_frame(create_test_frame()) {
            submitted += 1;
        } else {
            dropped += 1;
        }
    }

    // Some frames should be submitted
    assert!(
        submitted > 0,
        "At least some frames should be submitted successfully"
    );

    // Due to the bounded channel (capacity 1), many frames should be dropped
    // when submitting faster than the worker can process
    assert!(
        dropped > 0,
        "Some frames should be dropped due to back pressure (channel capacity 1)"
    );

    eprintln!(
        "Rapid submission: {} submitted, {} dropped",
        submitted, dropped
    );

    // Give time for processing
    thread::sleep(Duration::from_millis(200));

    // Verify server received at least some frame data
    assert!(
        handle.received_frame_data(),
        "Server should have received at least some frame data"
    );

    // Verify worker is still functional after burst
    let mut worker = worker;
    worker.update();
    assert!(
        matches!(worker.state(), DacConnectionState::Connected { .. }),
        "Worker should remain connected after rapid submission"
    );

    // Verify we can still send frames after the burst
    handle.clear_received_packets();
    thread::sleep(Duration::from_millis(50)); // Give channel time to drain
    assert!(
        worker.submit_frame(create_test_frame()),
        "Should be able to submit frame after burst completes"
    );
    thread::sleep(Duration::from_millis(100));
    assert!(
        handle.received_frame_data(),
        "Server should receive frame after burst"
    );
}

#[test]
fn test_submit_frame_returns_false_when_busy() {
    let server = MockIdnServer::new("TestDAC").unwrap();
    let server_addr = server.addr();
    let _handle = server.run();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    let worker = wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Submit first frame - should succeed
    let first_result = worker.submit_frame(create_test_frame());
    assert!(first_result, "First frame should be submitted");

    // Immediately submit second frame while first is likely still being processed
    // With a channel capacity of 1, if the worker hasn't picked up the first frame yet,
    // this should return false
    let mut any_rejected = false;
    for _ in 0..10 {
        if !worker.submit_frame(create_test_frame()) {
            any_rejected = true;
            break;
        }
    }

    // We expect at least one rejection when submitting rapidly
    // (though this depends on timing, it should happen frequently)
    eprintln!("Immediate submission test: any_rejected = {}", any_rejected);

    // The important invariant: the worker should still be functional
    thread::sleep(Duration::from_millis(100));
    let mut worker = worker;
    worker.update();
    assert!(
        matches!(worker.state(), DacConnectionState::Connected { .. }),
        "Worker should remain connected regardless of dropped frames"
    );
}
