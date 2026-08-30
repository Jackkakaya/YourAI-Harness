//! SkillProvider：技能/指令目录管理。

use crate::error::YourAiError;
use crate::future::BoxFuture;

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SkillContent {
    pub info: SkillInfo,
    /// 注入到模型上下文的指令文本
    pub instructions: String,
    /// 此技能提供的工具名
    pub tools: Vec<String>,
}

pub trait SkillProvider: Send + Sync {
    fn list(&self) -> BoxFuture<'_, Result<Vec<SkillInfo>, YourAiError>>;
    fn load(&self, id: &str) -> BoxFuture<'_, Result<SkillContent, YourAiError>>;
    fn register(&self, skill: SkillContent) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn unregister(&self, id: &str) -> BoxFuture<'_, Result<(), YourAiError>>;
}
