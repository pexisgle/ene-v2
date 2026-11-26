//! Thin Bevy ECS resource wrapper over the persistence layer in `ene-store`.

pub(crate) use super::error::Result;
pub(crate) use ene_store::{clients, memories, plugins, schedules, work};

/// Relational persistence resource shared across ECS systems and RPC handlers.
#[derive(Clone, Debug, bevy_ecs::prelude::Resource)]
pub(crate) struct Store(ene_store::Store);

impl Store {
    #[cfg(test)]
    pub(crate) fn open(path: &std::path::Path) -> Result<Self> {
        ene_store::Store::open(path).map(Self).map_err(Into::into)
    }

    pub(crate) fn open_with_grants(
        path: &std::path::Path,
        grants: ene_policy::CapabilityGrants,
    ) -> Result<Self> {
        ene_store::Store::open_with_grants(path, grants)
            .map(Self)
            .map_err(Into::into)
    }
}

impl std::ops::Deref for Store {
    type Target = ene_store::Store;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Host wall-clock resource for evaluating scheduled reminders.
#[derive(Clone, Debug, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct ScheduleClock(pub(crate) ene_store::ScheduleClock);

impl ScheduleClock {
    #[cfg(test)]
    pub(crate) fn fixed(zone: jiff::tz::TimeZone, now: jiff::Timestamp) -> Self {
        Self(ene_store::ScheduleClock::fixed(zone, now))
    }

    pub(crate) fn now(&self) -> jiff::Timestamp {
        self.0.now()
    }
}

impl std::ops::Deref for ScheduleClock {
    type Target = ene_store::ScheduleClock;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
