use super::*;

impl ApiClient {
    pub async fn snapshot(&self, subscription: Option<&str>) -> Result<Snapshot, Error> {
        let mut url = self.url(&["snapshot"])?;
        if let Some(id) = subscription {
            url.query_pairs_mut().append_pair("connection_id", id);
        }
        self.get(url).await
    }
    pub async fn companions(&self) -> Result<Vec<Companion>, Error> {
        self.get(self.url(&["companions"])?).await
    }
    pub async fn companion(&self, id: &str) -> Result<Companion, Error> {
        self.get(self.url(&["companions", id])?).await
    }
    pub async fn patch_companion(
        &self,
        id: &str,
        input: &CompanionPatch,
    ) -> Result<Companion, Error> {
        self.mutate(Method::PATCH, &["companions", id], Some(input), None)
            .await
    }
    pub async fn messages(
        &self,
        companion: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Page<Message>, Error> {
        let mut url = self.url(&["companions", companion, "messages"])?;
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        if let Some(limit) = limit {
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
        }
        self.get(url).await
    }
    pub async fn message(&self, companion: &str, id: &str) -> Result<Message, Error> {
        self.get(self.url(&["companions", companion, "messages", id])?)
            .await
    }
    pub async fn send_message(
        &self,
        companion: &str,
        input: &MessageInput,
        key: &IdempotencyKey,
    ) -> Result<MessageAcceptance, Error> {
        self.mutate(
            Method::POST,
            &["companions", companion, "messages"],
            Some(input),
            Some(key),
        )
        .await
    }
    pub async fn settings(&self) -> Result<Settings, Error> {
        self.get(self.url(&["settings"])?).await
    }
    pub async fn patch_settings(&self, input: &SettingsPatch) -> Result<Settings, Error> {
        self.mutate(Method::PATCH, &["settings"], Some(input), None)
            .await
    }
    pub async fn grants(&self, companion: Option<&str>) -> Result<Vec<DataTransferGrant>, Error> {
        let mut url = self.url(&["grants"])?;
        if let Some(id) = companion {
            url.query_pairs_mut().append_pair("companion_id", id);
        }
        self.get(url).await
    }
    pub async fn create_grant(
        &self,
        input: &GrantInput,
        key: &IdempotencyKey,
    ) -> Result<GrantResult, Error> {
        self.mutate(Method::POST, &["grants"], Some(input), Some(key))
            .await
    }
    pub async fn patch_grant(&self, id: &str, input: &GrantPatch) -> Result<GrantResult, Error> {
        self.mutate(Method::PATCH, &["grants", id], Some(input), None)
            .await
    }
    pub async fn revoke_grant(&self, id: &str) -> Result<(), Error> {
        self.mutate(Method::DELETE, &["grants", id], None::<&()>, None)
            .await
    }
    pub async fn register_credentials(
        &self,
        id: &str,
        input: &CredentialInput,
    ) -> Result<CredentialResult, Error> {
        self.mutate(Method::PUT, &["credentials", id], Some(input), None)
            .await
    }
    pub async fn revoke_credentials(&self, id: &str) -> Result<(), Error> {
        self.mutate(Method::DELETE, &["credentials", id], None::<&()>, None)
            .await
    }
    pub async fn check(
        &self,
        id: &str,
        input: &CheckInput,
        key: &IdempotencyKey,
    ) -> Result<CheckResult, Error> {
        self.request(
            Method::POST,
            self.url(&["connections", id, "check"])?,
            Some(input),
            Some(key),
            Duration::from_secs(95),
        )
        .await
    }
    pub async fn models(&self, id: &str, companion: Option<&str>) -> Result<Vec<Model>, Error> {
        let mut url = self.url(&["connections", id, "models"])?;
        if let Some(id) = companion {
            url.query_pairs_mut().append_pair("companion_id", id);
        }
        self.request(Method::GET, url, None::<&()>, None, Duration::from_secs(35))
            .await
    }
    pub async fn providers(&self) -> Result<Vec<ProviderInfo>, Error> {
        self.get(self.url(&["providers"])?).await
    }
    pub async fn reload_providers(&self) -> Result<Vec<ProviderInfo>, Error> {
        self.mutate(Method::POST, &["providers", "reload"], None::<&()>, None)
            .await
    }
    pub async fn enable_provider(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Vec<ProviderInfo>, Error> {
        self.mutate(
            Method::PUT,
            &["providers", id, "enabled"],
            Some(&ProviderEnabled { enabled }),
            None,
        )
        .await
    }
    pub async fn openapi(&self) -> Result<serde_json::Value, Error> {
        self.get(self.url(&["openapi.json"])?).await
    }
}
