use super::host_id;

const ID_A: &str = "0123abcdef0123456789abcdef012345\n";
const ID_B: &str = "fedc9876543210fedcba9876543210fe\n";

#[test]
fn no_machines_runner_prefix_starts_another_machines_runner_names() {
    // Host names where one is a prefix of the other, and two identical ones.
    for (a, b) in [("ci", "ci-2"), ("archlinux", "archlinux")] {
        let prefix_a = format!("runnerd-{}-", host_id(a, ID_A).unwrap());
        let name_b = format!("runnerd-{}-app-1-1792692722", host_id(b, ID_B).unwrap());
        assert!(
            !name_b.starts_with(&prefix_a),
            "{prefix_a} matches {name_b}"
        );
    }
}

#[test]
fn the_host_part_is_short_and_safe_in_runner_and_container_names() {
    assert_eq!(
        host_id("My.Very_Long-Hostname.example.org\n", ID_A).unwrap(),
        "my-very-long-hostnam-0123abcd"
    );
    assert_eq!(host_id("", ID_A).unwrap(), "host-0123abcd");
    assert_eq!(host_id("ci", "not-an-id"), None);
}
