use crate::{error, storage::table::Table};
use std::path::PathBuf;
use yourai_core::prelude::*;
pub struct LocalSkills(Table<SkillContent>);
impl LocalSkills {
    pub fn open(path: PathBuf) -> Result<Self, YourAiError> {
        Ok(Self(Table::open(path)?))
    }
}
impl SkillProvider for LocalSkills {
    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SkillInfo>, YourAiError>> {
        Box::pin(async {
            Ok(self
                .0
                .data
                .lock()
                .unwrap()
                .values()
                .map(|s| s.info.clone())
                .collect())
        })
    }
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<SkillContent, YourAiError>> {
        Box::pin(async move {
            self.0
                .data
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| error("skills", "unknown skill"))
        })
    }
    fn register<'a>(&'a self, skill: SkillContent) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            self.0.update(|d| {
                d.insert(skill.info.id.clone(), skill);
            })
        })
    }
    fn unregister<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            self.0.update(|d| {
                d.remove(id);
            })
        })
    }
}
