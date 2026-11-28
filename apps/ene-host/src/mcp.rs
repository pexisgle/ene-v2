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
