//! Hook matcher：支持精确匹配、`|` 分隔多选、正则、`*` 通配。
//!
//! 与 Claude Code 的 `matchesPattern` 逻辑一致。

use regex::Regex;
use std::sync::Arc;

/// 预编译的 matcher，避免每次 dispatch 重新编译正则。
#[derive(Debug, Clone)]
pub enum CompiledMatcher {
    /// 匹配全部（省略或 `"*"`）
    All,
    /// 精确匹配或 `|` 分隔多选
    Exact(Vec<String>),
    /// 正则
    Regex(Arc<Regex>),
}

impl CompiledMatcher {
    /// 从配置字符串编译；非法正则返回错误，供配置注册阶段 fail fast。
    pub fn try_compile(pattern: &str) -> Result<Self, String> {
        if pattern.is_empty() || pattern == "*" {
            return Ok(CompiledMatcher::All);
        }
        // 仅含字母数字 _ | → 精确匹配或 pipe 分隔
        if pattern
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '|')
        {
            let parts: Vec<String> = pattern
                .split('|')
                .map(|s| normalize_legacy_tool_name(s.trim()).to_string())
                .collect();
            return Ok(CompiledMatcher::Exact(parts));
        }
        // 否则作为正则
        Regex::new(pattern)
            .map(|regex| CompiledMatcher::Regex(Arc::new(regex)))
            .map_err(|error| format!("invalid hook matcher regex '{pattern}': {error}"))
    }

    /// 便捷编译。调用方处理不可信配置时应使用 [`Self::try_compile`]。
    pub fn compile(pattern: &str) -> Self {
        Self::try_compile(pattern)
            .unwrap_or_else(|_| CompiledMatcher::Exact(vec!["__invalid_regex__".to_string()]))
    }

    /// 测试是否匹配。
    pub fn matches(&self, query: &str) -> bool {
        match self {
            CompiledMatcher::All => true,
            CompiledMatcher::Exact(parts) => {
                parts.iter().any(|p| p == normalize_legacy_tool_name(query))
            }
            CompiledMatcher::Regex(re) => {
                re.is_match(query)
                    || legacy_tool_names(query)
                        .iter()
                        .any(|legacy| re.is_match(legacy))
            }
        }
    }
}

pub(crate) fn normalize_legacy_tool_name(name: &str) -> &str {
    match name {
        "Task" => "Agent",
        "KillShell" => "TaskStop",
        "AgentOutputTool" | "BashOutputTool" => "TaskOutput",
        _ => name,
    }
}

fn legacy_tool_names(canonical: &str) -> &'static [&'static str] {
    match canonical {
        "Agent" => &["Task"],
        "TaskStop" => &["KillShell"],
        "TaskOutput" => &["AgentOutputTool", "BashOutputTool"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_matcher() {
        let m = CompiledMatcher::compile("");
        assert!(m.matches("anything"));
        let m = CompiledMatcher::compile("*");
        assert!(m.matches("anything"));
    }

    #[test]
    fn exact_matcher() {
        let m = CompiledMatcher::compile("Write");
        assert!(m.matches("Write"));
        assert!(!m.matches("Read"));
    }

    #[test]
    fn pipe_matcher() {
        let m = CompiledMatcher::compile("Read|Write|Edit");
        assert!(m.matches("Read"));
        assert!(m.matches("Write"));
        assert!(m.matches("Edit"));
        assert!(!m.matches("Bash"));
    }

    #[test]
    fn regex_matcher() {
        let m = CompiledMatcher::compile("^mcp__.*");
        assert!(m.matches("mcp__weather__get"));
        assert!(!m.matches("Bash"));
    }

    #[test]
    fn legacy_tool_names_match_canonical_names() {
        assert!(CompiledMatcher::compile("Task").matches("Agent"));
        assert!(CompiledMatcher::compile("^Task$").matches("Agent"));
        assert!(CompiledMatcher::compile("KillShell").matches("TaskStop"));
    }

    #[test]
    fn invalid_regex_is_reported_by_strict_compiler() {
        assert!(CompiledMatcher::try_compile("[").is_err());
    }
}
