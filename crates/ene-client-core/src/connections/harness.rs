use crate::domain::{Connection, ConnectionKind, PluginInfo};
use ene_domain::settings::TASK_HARNESS_CAPABILITY;

/// Installed harness connections whose current configuration passed its check.
pub fn task_harness_choices(
    connections: &[Connection],
    plugins: &[PluginInfo],
) -> Vec<(String, String, String)> {
    connections
        .iter()
        .filter(|connection| connection.enabled && connection.kind == ConnectionKind::Acp)
        .filter_map(|connection| {
            let adapter = &connection.endpoint()?.adapter;
            let check = connection.last_check.as_ref()?;
            if !check.healthy
                || !check
                    .capabilities
                    .iter()
                    .any(|cap| cap == TASK_HARNESS_CAPABILITY)
                || !plugins
                    .iter()
                    .any(|plugin| &plugin.id == adapter && plugin.offers_harness())
            {
                return None;
            }
            Some((
                connection.id.clone(),
                adapter.clone(),
                format!("{}（ハーネス）", connection.name),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{connections::build_custom_connection_input, domain::ConnectionCheck};

    #[test]
    fn only_a_checked_enabled_harness_package_is_assignable() {
        let input =
            build_custom_connection_input(ConnectionKind::Acp, "手", "reference", "").unwrap();
        let mut connection = Connection {
            name: input.name,
            id: input.id,
            kind: input.kind,
            config: input.config,
            enabled: input.enabled,
            last_check: Some(ConnectionCheck {
                healthy: true,
                capabilities: vec![TASK_HARNESS_CAPABILITY.into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut plugin = PluginInfo {
            id: "reference".into(),
            enabled: true,
            extensions: vec![TASK_HARNESS_CAPABILITY.into()],
            ..Default::default()
        };
        let choices = task_harness_choices(
            std::slice::from_ref(&connection),
            std::slice::from_ref(&plugin),
        );
        assert_eq!(
            choices,
            [(
                connection.id.clone(),
                "reference".into(),
                "手（ハーネス）".into()
            )]
        );
        plugin.enabled = false;
        assert_eq!(
            task_harness_choices(
                std::slice::from_ref(&connection),
                std::slice::from_ref(&plugin)
            ),
            []
        );
        plugin.enabled = true;
        connection.last_check = None;
        assert_eq!(task_harness_choices(&[connection], &[plugin]), []);
    }
}
