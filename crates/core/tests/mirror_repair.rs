//! The mirror's repair, in the registry image's busybox on a scratch volume: what it removes and what it keeps. The
//! unit tests run the same script with the host's `sh`. Needs Docker; pulls the pinned `registry` image if missing:
//!
//! ```sh
//! cargo test -p runnerd-core --test mirror_repair -- --ignored --nocapture
//! ```

#![allow(
    clippy::print_stdout,
    reason = "run with --nocapture, the test shows what the repair reports"
)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "a test: Docker that does not answer fails it"
)]

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use runnerd_core::cache::mirror::{Fault, Repair, repair};
use runnerd_core::cache::{MIRROR_IMAGE, Names};
use runnerd_core::docker::{Docker, DockerApi};

/// A prefix no daemon uses, so the test never meets a real mirror's volume.
const PREFIX: &str = "runnerd-test-mirror-repair";

/// The digest of no bytes, whose data is rightly empty.
const NOTHING: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Lays out a registry's storage: blob `aa…` stored and linked from two repositories, `bb…` linked but gone, `cc…`
/// empty, and the blob of no bytes, empty. Links hold their digest without a newline, as the registry writes them.
fn layout() -> String {
    let (stored, gone, empty) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
    let blob = |hex: &str, data: &str| {
        format!(
            "mkdir -p blobs/sha256/{0}/{hex} && printf '{data}' > blobs/sha256/{0}/{hex}/data",
            &hex[..2]
        )
    };
    let link = |repository: &str, hex: &str| {
        format!(
            "mkdir -p repositories/{repository}/_layers/sha256/{hex} && \
             printf 'sha256:{hex}' > repositories/{repository}/_layers/sha256/{hex}/link"
        )
    };
    [
        "mkdir -p /var/lib/registry/docker/registry/v2 && cd /var/lib/registry/docker/registry/v2"
            .to_owned(),
        blob(&stored, "layer"),
        blob(&empty, ""),
        blob(NOTHING, ""),
        link("library/caddy", &stored),
        link("library/golang", &stored),
        link("library/golang", &gone),
        link("library/node", &empty),
        link("library/alpine", NOTHING),
    ]
    .join(" && ")
}

/// Which of the layout's files are left, as `ls` names them.
const LISTING: &str = "cd /var/lib/registry/docker/registry/v2 && \
    find repositories blobs -type f | sort";

/// Runs `script` in the registry image on the scratch volume; what it printed.
async fn shell(docker: &Docker, names: &Names, script: &str) -> String {
    let name = format!("{PREFIX}-shell");
    let _ = docker.remove(&name).await;
    let body = ContainerCreateBody {
        image: Some(MIRROR_IMAGE.to_owned()),
        entrypoint: Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
        ]),
        host_config: Some(HostConfig {
            network_mode: Some("none".to_owned()),
            mounts: Some(vec![Mount {
                target: Some("/var/lib/registry".to_owned()),
                source: Some(names.mirror()),
                typ: Some(MountType::VOLUME),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    docker.run(&name, body).await.expect("the shell starts");
    let code = docker.wait(&name).await.expect("the shell ends");
    let output = docker.logs(&name).await.expect("its output is read");
    docker.remove(&name).await.expect("the shell is removed");
    assert_eq!(code, 0, "the shell exits 0: {output}");
    output
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_repair_in_the_registry_image_removes_what_serves_no_data_and_keeps_the_rest() {
    let docker = Docker::connect().expect("Docker answers");
    let names = Names::new(PREFIX);
    if !docker
        .image_present(MIRROR_IMAGE)
        .await
        .expect("Docker lists images")
    {
        docker
            .pull(MIRROR_IMAGE)
            .await
            .expect("the registry image pulls");
    }
    let _ = docker.remove_volume(&names.mirror()).await;
    shell(&docker, &names, &layout()).await;

    let first = repair(&docker, &names, "system.slice").await;
    let second = repair(&docker, &names, "system.slice").await;
    let left = shell(&docker, &names, LISTING).await;
    docker
        .remove_volume(&names.mirror())
        .await
        .expect("nothing holds the scratch volume");
    println!("{first:?}\n{left}");

    let Repair::Removed(links) = first else {
        panic!("the first repair removes links: {first:?}");
    };
    assert_eq!(links.count, 5);
    let mut faults: Vec<(String, Fault)> = links
        .dangling
        .iter()
        .map(|dangling| (dangling.repository.clone(), dangling.fault))
        .collect();
    faults.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        faults,
        [
            ("library/golang".to_owned(), Fault::Gone),
            ("library/node".to_owned(), Fault::Empty),
        ]
    );
    assert_eq!(second, Repair::Clean, "a second repair finds nothing");
    let (stored, nothing) = ("a".repeat(64), NOTHING);
    let expected = [
        format!("blobs/sha256/aa/{stored}/data"),
        format!("blobs/sha256/e3/{nothing}/data"),
        format!("repositories/library/alpine/_layers/sha256/{nothing}/link"),
        format!("repositories/library/caddy/_layers/sha256/{stored}/link"),
        format!("repositories/library/golang/_layers/sha256/{stored}/link"),
    ];
    assert_eq!(
        left.lines().collect::<Vec<_>>(),
        expected,
        "the stored blob, the blob of no bytes and their links stay; the rest, the empty data too, goes"
    );
}
