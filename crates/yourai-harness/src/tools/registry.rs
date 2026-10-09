use std::{collections::HashMap, sync::RwLock};
use yourai_core::prelude::*;
#[derive(Default)]
pub struct ToolSet {
    tools: RwLock<HashMap<String, Tool>>,
}
impl ToolRegistry for ToolSet {
    fn register(&self, provider: std::sync::Arc<dyn ToolProvider>) {
        let tool = Tool::new(provider);
        self.tools.write().unwrap().insert(tool.name().into(), tool);
    }
    fn unregister(&self, name: &str) {
        self.tools.write().unwrap().remove(name);
    }
    fn snapshot(&self) -> Vec<Tool> {
        let mut tools: Vec<_> = self.tools.read().unwrap().values().cloned().collect();
        tools.sort_by(|a, b| a.name().cmp(b.name()));
        tools
    }
}

impl ToolSet {
    pub fn new(tools: Vec<Tool>) -> Self {
        Self {
            tools: RwLock::new(
                tools
                    .into_iter()
                    .map(|tool| (tool.name().to_owned(), tool))
                    .collect(),
            ),
        }
    }
}
