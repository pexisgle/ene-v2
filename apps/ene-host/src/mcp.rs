use crate::process::Child;
use crate::{
    model::{Models, check_endpoint},
    store,
};
use anyhow::{Result, ensure};
use ene_rpc::{Connection, ProviderKind};
use rmcp::{
    RoleClient, ServiceExt as _,
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, async_rw::AsyncRwTransport,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(crate) fn permits(connection: &Connection, name: &str) -> bool {
    connection
        .mcp_tools
        .as_option()
        .is_some_and(|scope| scope.all || scope.names.iter().any(|allowed| allowed == name))
}

pub(crate) fn validate_scope(connection: &Connection) -> Result<()> {
    if let Some(scope) = connection.mcp_tools.as_option() {
        ensure!(
            !scope.all || scope.names.is_empty(),
            "a tool scope must select all or list tools"
        );
        ensure!(
            scope.names.len() <= 256
                && scope
                    .names
                    .iter()
                    .all(|name| !name.trim().is_empty() && name.len() <= 256),
            "invalid MCP tool names"
        );
    }
    Ok(())
}

pub(crate) struct Mcp {
    pub connection: Connection,
    pub service: RunningService<RoleClient, ()>,
    _child: Option<Child>,
}

impl Mcp {
    pub(crate) async fn open(
        connection: Connection,
        models: &Models,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        ensure!(connection.enabled, "MCP connection disabled");
        let (service, child) = tokio::time::timeout(Duration::from_secs(20), async {
            match connection.kind.as_known() {
                Some(ProviderKind::McpStdio) => {
                    let (child, input, output) =
                        crate::process::spawn(&connection.command, &connection.args)?;
                    let service =
                        ().serve_with_ct(
                            AsyncRwTransport::new(output, input),
                            cancel.child_token(),
                        )
                        .await?;
                    Ok((service, Some(child)))
                }
                Some(ProviderKind::McpHttp) => {
                    check_endpoint(&connection.endpoint)?;
                    let credential = models.credential(&connection).await?;
                    let config =
                        StreamableHttpClientTransportConfig::with_uri(connection.endpoint.as_str());
                    let config = if let Some(secret) = credential {
                        config.auth_header(secret)
                    } else {
                        config
                    };
                    let http = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .no_proxy()
                        .timeout(Duration::from_secs(60))
                        .build()?;
                    let transport = StreamableHttpClientTransport::with_client(http, config);
                    Ok((
                        ().serve_with_ct(transport, cancel.child_token()).await?,
                        None,
                    ))
                }
                _ => anyhow::bail!("connection is not an MCP server"),
            }
        })
        .await??;
        Ok(Self {
            connection,
            service,
            _child: child,
        })
    }

    pub(crate) fn receiver_revision(&self, tool: &rmcp::model::Tool) -> Result<String> {
        store::fingerprint(&(
            &self.connection.id,
            self.connection.revision,
            tool.name.as_ref(),
            &tool.input_schema,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicitly_selected_mcp_tools_are_exposed() {
        let mut connection = Connection::default();
        assert!(!permits(&connection, "read"));
        connection.mcp_tools = Some(ene_rpc::McpToolScope {
            names: vec!["read".into()],
            ..Default::default()
        })
        .into();
        assert!(validate_scope(&connection).is_ok());
        assert!(permits(&connection, "read"));
        assert!(!permits(&connection, "write"));
        let mut scope = connection.mcp_tools.as_option().expect("scope").clone();
        scope.all = true;
        connection.mcp_tools = Some(scope.clone()).into();
        assert!(validate_scope(&connection).is_err());
        scope.names.clear();
        connection.mcp_tools = Some(scope).into();
        assert!(validate_scope(&connection).is_ok());
        assert!(permits(&connection, "write"));
    }
}
