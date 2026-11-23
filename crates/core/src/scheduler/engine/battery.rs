//! Whether the host runs on its battery, as the engine last read it, and the hold `[host] on_battery = "pause"` puts
//! on every start while it does (ADR 0161). The power supplies are read before each plan that may start
//! runners, a tick's and a pass's (`observe::power`). A hold of the host's, not the pause the store
//! keeps per repository: it says nothing of a repository, and ends by itself once the host is on mains power again.

use tracing::info;

use super::Engine;
use crate::config::OnBattery;
use crate::docker::DockerApi;
use crate::github::GithubApi;
use crate::observe::power::{self, Power};

/// What powers the host, from the last tick's read: mains until the first.
///
/// Nothing of it survives a restart or a reload, which read the supplies again at their first tick.
#[derive(Debug, Default)]
pub(super) struct Battery {
    power: Power,
}

impl Battery {
    /// Takes `power`, read now, and says whether it changed.
    fn read(&mut self, power: Power) -> bool {
        std::mem::replace(&mut self.power, power) != power
    }

    /// Whether `on_battery` holds every start back as the host is powered now.
    fn holds(&self, on_battery: OnBattery) -> bool {
        self.power == Power::Battery && on_battery == OnBattery::Pause
    }
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Reads what powers the host, for the plan that follows, tells the job actions whether it holds starts, and says
    /// so in the log when it changed.
    pub(super) fn look_at_power(&mut self) {
        let power = power::read(&self.paths.sys);
        let changed = self.battery.read(power);
        // Every tick: a reload may have changed `on_battery`.
        self.control.set_on_battery(self.on_battery());
        if !changed {
            return;
        }
        match (power, self.config.host.on_battery) {
            (Power::Battery, OnBattery::Pause) => info!(
                "the host is on battery: no runner starts and idle runners stop until it is plugged in, as \
                 [host] on_battery = \"pause\"; busy runners finish their jobs"
            ),
            (Power::Battery, OnBattery::Run) => {
                info!(
                    "the host is on battery: runners start as on mains power, as [host] on_battery = \"run\""
                );
            }
            (Power::Mains, _) => info!("the host is on mains power"),
        }
    }

    /// Whether the host on battery holds every start back (`[host] on_battery = "pause"`).
    pub(super) fn on_battery(&self) -> bool {
        self.battery.holds(self.config.host.on_battery)
    }
}
