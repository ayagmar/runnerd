use super::HostIdentity;

const ID_A: &str = "0123abcdef0123456789abcdef012345\n";
const ID_B: &str = "fedc9876543210fedcba9876543210fe\n";

fn identity(hostname: &str, machine_id: &str) -> HostIdentity {
    HostIdentity::new(hostname, machine_id).unwrap()
}

#[test]
fn no_machines_runner_prefix_starts_another_machines_runner_names() {
    // Host names where one is a prefix of the other, and two identical ones.
    for (a, b) in [("ci", "ci-2"), ("archlinux", "archlinux")] {
        let prefix = identity(a, ID_A).prefix();
        let name = identity(b, ID_B).runner_name("app", 1, 1_792_692_722);
        assert!(!name.starts_with(&prefix), "{prefix} matches {name}");
    }
}

#[test]
fn a_new_host_name_still_matches_the_runners_named_before() {
    let before = identity("laptop", ID_A).runner_name("app", 1, 1_792_692_722);
    let after = identity("laptop-dhcp-42", ID_A);
    assert!(before.starts_with(&after.prefix()), "{before}");
}

#[test]
fn runner_names_are_short_safe_and_hide_the_machine_id() {
    let name = identity("My.Very_Long-Hostname.example.org\n", ID_A).runner_name("app", 2, 7);
    let id = &name["runnerd-".len().."runnerd-".len() + 8];
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
    assert!(!ID_A.contains(id), "the machine ID shows in {name}");
    assert_eq!(name, format!("runnerd-{id}-my-very-long-hostnam-app-2-7"));
    assert_eq!(
        identity("", ID_A).runner_name("app", 2, 7),
        format!("runnerd-{id}-host-app-2-7")
    );
    assert_eq!(HostIdentity::new("ci", "not-an-id"), None);
}
