use crate::{
    error::{conflict, denied, invalid, missing},
    model::{Models, check_endpoint},
    plugin_runtime::{
        Package,
        instance::{Instance, Services},
        service::{HttpBroker, HttpGrants},
    },
    store,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ene_plugin::{
    Event, Failure, HttpRequest, Operation, ServiceRequest,
    provider::{AuthProfile, BrokerRouting},
};
use ene_rpc::{Connection, Plugin};
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use rig_core::http_client::{
    BoxedStream, Bytes, Error as HttpError, HttpClientExt, LazyBody, MultipartForm, Request,
    Response, StreamingResponse,
};
use serde_json::Value;
use sqlx::SqlitePool;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone)]
pub(crate) struct PluginProvider {
    db: SqlitePool,
    package: Package,
    plugin: Plugin,
    connection: Connection,
    grant: Arc<Grant>,
    pub options: Value,
}

impl PluginProvider {
    pub(crate) async fn open(
        db: SqlitePool,
        models: &Models,
        connection: Connection,
    ) -> Result<Self> {
        let mut read = db.acquire().await?;
        let plugin: Plugin = store::get(&mut read, "plugin", &connection.plugin_id)
            .await?
            .filter(|plugin: &Plugin| plugin.enabled)
            .ok_or_else(|| missing("provider plugin disabled or missing"))?;
        drop(read);
        let path = std::path::PathBuf::from(&plugin.path);
        let package = tokio::task::spawn_blocking(move || Package::load(&path)).await??;
        if package.revision != plugin.package_digest || package.manifest.id != plugin.id {
            return Err(conflict(
                "plugin package changed; reinstall to approve its capabilities",
            ));
        }
        let provider = package
            .entry(&connection.entrypoint)?
            .provider
            .as_ref()
            .ok_or_else(|| invalid("plugin entrypoint has no provider"))?;
        let endpoint = check_endpoint(&connection.endpoint)?;
        if !provider
            .destinations
            .iter()
            .filter_map(|url| check_endpoint(url).ok())
            .any(|url| url.origin() == endpoint.origin())
        {
            return Err(denied(
                "connection endpoint is outside the plugin's declared destinations",
            ));
        }
        if !matches!(provider.routing, BrokerRouting::Direct) {
            return Err(invalid("unsupported plugin routing configuration"));
        }
        let credential = models.credential(&connection).await?;
        let mut headers = HeaderMap::new();
        for (name, value) in &provider.headers {
            let name = HeaderName::from_bytes(name.as_bytes())?;
            if matches!(
                name.as_str(),
                "host" | "authorization" | "proxy-authorization" | "cookie" | "content-length"
            ) {
                return Err(invalid("plugin contains a protected HTTP header"));
            }
            headers.insert(name, HeaderValue::from_str(value)?);
        }
        match (&provider.auth, credential) {
            (AuthProfile::Bearer, Some(secret)) => {
                let mut value = HeaderValue::from_str(&format!("Bearer {secret}"))?;
                value.set_sensitive(true);
                headers.insert(http::header::AUTHORIZATION, value);
            }
            (AuthProfile::Header { name }, Some(secret)) => {
                let mut value = HeaderValue::from_str(&secret)?;
                value.set_sensitive(true);
                headers.insert(HeaderName::from_bytes(name.as_bytes())?, value);
            }
            (AuthProfile::Query { .. }, _) => {
                return Err(invalid("query credentials are unsupported"));
            }
            _ => {}
        }
        let mut options = provider.options.as_object().cloned().unwrap_or_default();
        if !connection.options_json.is_empty() {
            options.extend(serde_json::from_str::<serde_json::Map<String, Value>>(
                &connection.options_json,
            )?);
        }
        let grant = Arc::new(Grant {
            endpoint,
            headers,
            provider: provider.id.clone(),
        });
        Ok(Self {
            db,
            package,
            plugin,
            connection,
            grant,
            options: options.into(),
        })
    }

    pub(crate) async fn invoke(
        &self,
        operation: Operation,
        cancel: &CancellationToken,
    ) -> Result<(Value, Vec<Event>)> {
        let mut instance = Instance::load(
            self.package.clone(),
            self.connection.entrypoint.clone(),
            cancel,
        )
        .await?;
        let services = ProviderServices {
            provider: self.clone(),
            broker: HttpBroker::with_lifetime(self.grant.clone(), None, cancel.child_token())?,
        };
        let (sender, mut receiver) = mpsc::channel(32);
        let result = instance.invoke(operation, Arc::new(services), sender, cancel);
        let collect = async {
            let mut events = Vec::new();
            while let Some(event) = receiver.recv().await {
                events.push(event);
            }
            events
        };
        let (result, events) = tokio::join!(result, collect);
        Ok((result?, events))
    }

    pub(crate) async fn models(&self) -> Result<Vec<String>> {
        let cancel = CancellationToken::new();
        let _cancel = cancel.clone().drop_guard();
        let (_, events) = self
            .invoke(
                Operation::ListModels {
                    endpoint: self.connection.endpoint.clone(),
                    options: self.options.clone(),
                },
                &cancel,
            )
            .await?;
        Ok(events
            .into_iter()
            .flat_map(|event| match event {
                Event::Models { models } => models,
                _ => Vec::new(),
            })
            .map(|model| model.id)
            .collect())
    }

    async fn exchange(&self, request: Request<Bytes>) -> Result<(StatusCode, HeaderMap, Bytes)> {
        let (parts, bytes) = request.into_parts();
        let url = if parts.uri.scheme().is_some() {
            parts.uri.to_string()
        } else {
            self.grant
                .endpoint
                .join(parts.uri.path_and_query().map_or("/", |path| path.as_str()))?
                .to_string()
        };
        let headers = parts
            .headers
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "authorization" | "proxy-authorization" | "cookie" | "host" | "content-length"
                )
            })
            .filter_map(|(key, value)| Some((key.as_str().into(), value.to_str().ok()?.into())))
            .collect::<BTreeMap<_, _>>();
        let cancel = CancellationToken::new();
        let _cancel = cancel.clone().drop_guard();
        let (_, events) = self
            .invoke(
                Operation::GenerateHttp {
                    request: HttpRequest {
                        method: parts.method.as_str().into(),
                        url,
                        headers,
                        body_base64: Some(STANDARD.encode(bytes)),
                        stream: false,
                    },
                },
                &cancel,
            )
            .await?;
        let mut status = None;
        let mut headers = HeaderMap::new();
        let mut body = Vec::new();
        let mut finished = false;
        for event in events {
            let Event::Http { chunk } = event else {
                anyhow::bail!("invalid plugin response");
            };
            ensure!(!finished, "unexpected response after terminal chunk");
            if status.is_none() {
                status = Some(StatusCode::from_u16(chunk.status)?);
                for (name, value) in chunk.headers {
                    if matches!(name.as_str(), "set-cookie" | "transfer-encoding") {
                        continue;
                    }
                    headers.insert(
                        HeaderName::from_bytes(name.as_bytes())?,
                        HeaderValue::from_str(&value)?,
                    );
                }
            }
            body.extend(STANDARD.decode(chunk.data_base64)?);
            ensure!(body.len() <= 16 * 1024 * 1024, "plugin response too large");
            finished = chunk.done;
        }
        ensure!(finished, "plugin response did not finish");
        Ok((
            status.context("missing plugin status")?,
            headers,
            Bytes::from(body),
        ))
    }
}

struct Grant {
    endpoint: Url,
    headers: HeaderMap,
    provider: String,
}

impl HttpGrants for Grant {
    fn allows(&self, provider: &str, url: &Url) -> bool {
        provider == self.provider
            && ene_core::policy::safe_url(url)
            && url.origin() == self.endpoint.origin()
            && url
                .path()
                .starts_with(self.endpoint.path().trim_end_matches('/'))
    }

    fn authorize(&self, provider: &str, headers: &mut HeaderMap) -> Result<(), Failure> {
        if provider != self.provider {
            return Err(Failure::new("wrong_provider"));
        }
        headers.extend(self.headers.clone());
        Ok(())
    }
}

struct ProviderServices {
    provider: PluginProvider,
    broker: HttpBroker,
}

impl Services for ProviderServices {
    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move {
            let mut db = self
                .provider
                .db
                .acquire()
                .await
                .map_err(|_| Failure::new("state_unavailable"))?;
            let connection: Option<Connection> =
                store::get(&mut db, "connection", &self.provider.connection.id)
                    .await
                    .map_err(|_| Failure::new("state_unavailable"))?;
            let plugin: Option<Plugin> = store::get(&mut db, "plugin", &self.provider.plugin.id)
                .await
                .map_err(|_| Failure::new("state_unavailable"))?;
            if !connection.is_some_and(|selected| {
                selected.enabled && selected.revision == self.provider.connection.revision
            }) || !plugin.is_some_and(|plugin| {
                plugin.enabled && plugin.revision == self.provider.plugin.revision
            }) {
                return Err(Failure::new("provider_changed"));
            }
            self.broker.check()
        })
    }

    fn handle(&self, request: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        Box::pin(async move {
            self.check().await?;
            let chunk = match request {
                ServiceRequest::Configuration => return Ok(self.provider.options.clone()),
                ServiceRequest::HttpOpen { request } => {
                    self.broker
                        .open(&self.provider.grant.provider, request)
                        .await?
                }
                ServiceRequest::HttpRead { handle } => self.broker.read(&handle).await?,
                ServiceRequest::HttpClose { handle } => {
                    self.broker.close(&handle)?;
                    return Ok(Value::Null);
                }
                _ => return Err(Failure::new("service_denied")),
            };
            serde_json::to_value(chunk).map_err(|_| Failure::new("invalid_response"))
        })
    }
}

impl HttpClientExt for PluginProvider {
    fn send(
        &self,
        request: Request<Bytes>,
    ) -> impl std::future::Future<Output = Result<Response<LazyBody>, HttpError>> + Send + 'static
    {
        let provider = self.clone();
        async move {
            let (status, headers, body) = provider
                .exchange(request)
                .await
                .map_err(|_| plugin_error())?;
            let mut response = Response::new(Box::pin(async move { Ok(body) }) as LazyBody);
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            Ok(response)
        }
    }

    fn send_multipart(
        &self,
        _: Request<MultipartForm>,
    ) -> impl std::future::Future<Output = Result<Response<LazyBody>, HttpError>> + Send + 'static
    {
        std::future::ready(Err(plugin_error()))
    }

    async fn send_streaming(
        &self,
        request: Request<Bytes>,
    ) -> Result<StreamingResponse, HttpError> {
        let (status, headers, body) = self.exchange(request).await.map_err(|_| plugin_error())?;
        let mut response =
            Response::new(Box::pin(futures_util::stream::iter([Ok(body)])) as BoxedStream);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

fn plugin_error() -> HttpError {
    HttpError::permanent(std::io::Error::other("plugin exchange failed"))
}
