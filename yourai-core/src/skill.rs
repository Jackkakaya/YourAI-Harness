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
    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SkillInfo>, YourAiError>>;
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<SkillContent, YourAiError>>;
    fn register<'a>(&'a self, skill: SkillContent) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn unregister<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), YourAiError>>;
}
