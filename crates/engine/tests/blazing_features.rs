//! End-to-end tests for BlazingDL engine features:
//! 1. Multi-NIC bandwidth aggregation (Plexo-style)
//! 2. Multi-mirror racing & automatic failover
//! 3. Hedged tail racing & adaptive concurrency
//! 4. Metalink v4 (.meta4) mirror parsing + verified download

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use blazingdl_engine::net::{Binding, Route};
use blazingdl_engine::{Destination, Download, Options, Request, metalink, verify};
use blazingdl_labserver::{Config, Faults, LabServer, content};

async fn server(faults: &str) -> LabServer {
    LabServer::start(Config {
        faults: Faults::parse(faults).unwrap(),
        ..Config::default()
    })
    .await
    .unwrap()
}

fn fast_opts() -> Options {
    Options {
        retry_delay: Duration::from_millis(40),
        max_connections: 6,
        ..Options::default()
    }
}

#[tokio::test]
async fn multi_nic_aggregate_distributes_workers_and_reports_nic_speeds() {
    let s = server("rate=4M").await;
    let dir = tempfile::tempdir().unwrap();

    // Two distinct bindings representing Ethernet + Wi-Fi (loopback_too: false allows localhost).
    let nic_a = Arc::new(Binding {
        label: "Ethernet".into(),
        name: "Ethernet".into(),
        index: 10,
        v4: Some(Ipv4Addr::new(192, 168, 10, 2)),
        v6: None,
        loopback_too: false,
    });
    let nic_b = Arc::new(Binding {
        label: "Wi-Fi".into(),
        name: "Wi-Fi".into(),
        index: 11,
        v4: Some(Ipv4Addr::new(192, 168, 20, 2)),
        v6: None,
        loopback_too: false,
    });

    let opts = Options {
        route: Route::Aggregate(vec![nic_a, nic_b]),
        max_connections: 6,
        ..fast_opts()
    };
    let req = Request {
        url: s.url("/gen/4M/bond.bin?seed=42").parse().unwrap(),
        headers: Default::default(),
        dest: Destination::Dir(dir.path().to_owned()),
    };

    let dl = Download::start(req, opts);
    let mut saw_both_nics = false;
    for _ in 0..200 {
        let snap = dl.snapshot();
        if snap.nic_speeds.len() == 2 && snap.nic_speeds.iter().all(|n| n.downloaded > 0) {
            saw_both_nics = true;
        }
        if matches!(
            snap.state,
            blazingdl_engine::download::State::Done
                | blazingdl_engine::download::State::Failed
                | blazingdl_engine::download::State::Cancelled
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let out = dl.wait().await.expect("multi-NIC download succeeds");
    let final_saw_both = saw_both_nics;

    let data = std::fs::read(&out.path).unwrap();
    let mut want = vec![0u8; data.len()];
    content::fill(42, 0, &mut want);
    assert_eq!(data, want, "multi-NIC downloaded bytes match generator");
    assert!(final_saw_both, "both aggregated NICs transferred bytes");
}

#[tokio::test]
async fn multi_mirror_races_chunks_across_both_servers() {
    let primary = server("rate=3M").await;
    let mirror = server("rate=3M").await;
    let dir = tempfile::tempdir().unwrap();

    let opts = Options {
        mirrors: vec![mirror.url("/gen/4M/race.bin?seed=77").parse().unwrap()],
        max_connections: 6,
        ..fast_opts()
    };
    let req = Request {
        url: primary.url("/gen/4M/race.bin?seed=77").parse().unwrap(),
        headers: Default::default(),
        dest: Destination::Dir(dir.path().to_owned()),
    };

    let out = Download::start(req, opts)
        .wait()
        .await
        .expect("multi-mirror racing succeeds");
    let data = std::fs::read(&out.path).unwrap();
    let mut want = vec![0u8; data.len()];
    content::fill(77, 0, &mut want);
    assert_eq!(data, want);

    tokio::time::timeout(Duration::from_secs(10), primary.settled())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), mirror.settled())
        .await
        .unwrap();
    assert!(
        !primary.log().is_empty() && !mirror.log().is_empty(),
        "expected both primary ({}) and mirror ({}) to serve chunks",
        primary.log().len(),
        mirror.log().len()
    );
}

#[tokio::test]
async fn multi_mirror_fails_over_when_mirror_is_broken() {
    let primary = server("rate=4M").await;
    // Mirror fails every request; workers must cool it down and complete via primary.
    let bad_mirror = server("fail_every=1").await;
    let dir = tempfile::tempdir().unwrap();

    let opts = Options {
        mirrors: vec![
            bad_mirror
                .url("/gen/2M/failover.bin?seed=99")
                .parse()
                .unwrap(),
        ],
        max_connections: 4,
        ..fast_opts()
    };
    let req = Request {
        url: primary.url("/gen/2M/failover.bin?seed=99").parse().unwrap(),
        headers: Default::default(),
        dest: Destination::Dir(dir.path().to_owned()),
    };

    let out = Download::start(req, opts)
        .wait()
        .await
        .expect("download completes despite failing mirror");
    let data = std::fs::read(&out.path).unwrap();
    let mut want = vec![0u8; data.len()];
    content::fill(99, 0, &mut want);
    assert_eq!(data, want);
}

#[tokio::test]
async fn hedged_tail_and_adaptive_streams_complete_cleanly() {
    let s = server("rate=3M").await;
    let dir = tempfile::tempdir().unwrap();

    let opts = Options {
        hedge_tail: true,
        adaptive_streams: true,
        max_connections: 8,
        ..fast_opts()
    };
    let req = Request {
        url: s.url("/gen/4M/hedged.bin?seed=123").parse().unwrap(),
        headers: Default::default(),
        dest: Destination::Dir(dir.path().to_owned()),
    };

    let out = Download::start(req, opts)
        .wait()
        .await
        .expect("hedged adaptive download succeeds");
    let data = std::fs::read(&out.path).unwrap();
    let mut want = vec![0u8; data.len()];
    content::fill(123, 0, &mut want);
    assert_eq!(data, want);
}

#[tokio::test]
async fn metalink_v4_downloads_from_mirrors_and_verifies_sha256() {
    let m1 = server("rate=4M").await;
    let m2 = server("rate=4M").await;
    let dir = tempfile::tempdir().unwrap();

    let mut expected_bytes = vec![0u8; 512 * 1024];
    content::fill(55, 0, &mut expected_bytes);
    let expected_sha256 = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&expected_bytes))
    };

    let u1 = m1.url("/gen/512K/distro.iso?seed=55");
    let u2 = m2.url("/gen/512K/distro.iso?seed=55");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <metalink xmlns="urn:ietf:params:xml:ns:metalink">
          <file name="distro.iso">
            <size>{}</size>
            <hash type="sha-256">{expected_sha256}</hash>
            <url priority="1">{u1}</url>
            <url priority="2">{u2}</url>
          </file>
        </metalink>"#,
        expected_bytes.len()
    );

    let files = metalink::parse(&xml).expect("valid metalink");
    assert_eq!(files.len(), 1);
    let mf = &files[0];
    let primary_url = mf.primary_url().expect("primary url").clone();
    let mirrors = mf.extra_mirrors();

    let req = Request {
        url: primary_url,
        headers: Default::default(),
        dest: Destination::Dir(dir.path().to_owned()),
    };
    let opts = Options {
        mirrors,
        ..fast_opts()
    };
    let out = Download::start(req, opts)
        .wait()
        .await
        .expect("metalink download succeeds");

    let expected = mf.best_hash().expect("sha256 present");
    let actual = verify::digest(&out.path, expected.algo).unwrap();
    assert_eq!(
        actual, expected.value,
        "metalink sha-256 matches downloaded file"
    );
}
