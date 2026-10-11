pub(crate) mod broker;
pub mod manifest;
pub mod process;
mod sandbox;
#[cfg(test)]
mod tests;
pub struct Provider {
    pub bundle: manifest::Bundle,
    pub supervisor: process::Supervisor,
}
impl ene_core::ports::llm::LlmProvider for Provider {
    fn policy(
        &self,
        c: &ene_core::domain::Connection,
    ) -> ene_core::ports::Result<ene_core::ports::llm::Policy> {
        self.bundle.policy(c)
    }
    fn generate(
        &self,
        i: ene_core::ports::llm::Invocation,
    ) -> futures_util::future::BoxFuture<
        '_,
        ene_core::ports::Result<
            futures_util::stream::BoxStream<
                'static,
                ene_core::ports::Result<ene_core::ports::llm::Event>,
            >,
        >,
    > {
        Box::pin(process::generate(
            self.bundle.clone(),
            i,
            self.supervisor.clone(),
        ))
    }
    fn models(
        &self,
        i: ene_core::ports::llm::Invocation,
    ) -> futures_util::future::BoxFuture<
        '_,
        ene_core::ports::Result<Vec<ene_core::ports::llm::Model>>,
    > {
        Box::pin(process::models(
            self.bundle.clone(),
            i,
            self.supervisor.clone(),
        ))
    }
}
