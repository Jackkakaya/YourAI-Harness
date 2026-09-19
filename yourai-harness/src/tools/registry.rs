use crate::error;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use yourai_core::prelude::*;
#[derive(Default)]
pub struct ToolSet {
    handlers: RwLock<HashMap<String, Arc<dyn ToolHandler>>>,
}
impl ToolRegistry for ToolSet {
    fn register(&self, h: Arc<dyn ToolHandler>) {
        self.handlers.write().unwrap().insert(h.name().into(), h);
    }
    fn unregister(&self, n: &str) {
        self.handlers.write().unwrap().remove(n);
    }
    fn has(&self, n: &str) -> bool {
        self.handlers.read().unwrap().contains_key(n)
    }
    fn definitions(&self) -> Vec<Tool> {
        let mut v: Vec<_> = self
            .handlers
            .read()
            .unwrap()
            .values()
            .map(|h| h.definition())
            .collect();
        v.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        v
    }
    fn resolve(&self, n: &str) -> Result<Arc<dyn ToolHandler>, YourAiError> {
        self.handlers
            .read()
            .unwrap()
            .get(n)
            .cloned()
            .ok_or_else(|| error("tools", format!("unknown tool: {n}")))
    }
    fn count(&self) -> usize {
        self.handlers.read().unwrap().len()
    }
}
