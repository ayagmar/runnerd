//! Whether the host runs on its battery, from the power supplies the kernel lists under
//! `/sys/class/power_supply` (ADR 0161). It reads files and decides nothing about scheduling.
//!
//! The host is on battery when a system battery, of `type` `Battery` with no `scope` or `scope` `System`, says
//! `status` `Discharging`. `Not charging` and `Full` are on mains power: a battery held at a charge limit says so. No
//! `Mains` supply is read, as a laptop that charges only over USB-C has none. A device's battery (a mouse's, `scope`
//! `Device`) is not the host's, and a host with no system battery, or no directory to list, is never on battery. A
//! file that cannot be read says nothing, so a supply whose `type` or `status` cannot be read does not count.

use std::fs;
use std::path::Path;

/// What powers the host, as its batteries say.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Power {
    /// Mains power, or no system battery to say otherwise.
    #[default]
    Mains,
    /// A system battery discharges.
    Battery,
}

/// What powers the host, from the power supplies under `sys` (usually `/sys`).
pub fn read(sys: &Path) -> Power {
    let Ok(supplies) = fs::read_dir(sys.join("class/power_supply")) else {
        return Power::Mains;
    };
    let discharging = supplies.flatten().any(|supply| {
        let path = supply.path();
        let field = |name: &str| fs::read_to_string(path.join(name)).ok();
        let system = match field("scope") {
            None => true,
            Some(scope) => scope.trim() == "System",
        };
        field("type").is_some_and(|kind| kind.trim() == "Battery")
            && system
            && field("status").is_some_and(|status| status.trim() == "Discharging")
    });
    if discharging {
        Power::Battery
    } else {
        Power::Mains
    }
}
