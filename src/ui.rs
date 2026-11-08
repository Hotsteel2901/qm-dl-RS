//! 终端 UI 小工具：统一颜色与提示前缀。
//!
//! `console` 会自动检测 TTY / NO_COLOR / TERM=dumb，管道和重定向时会自动退化为纯文本。

use console::style;

pub fn id(s: &str) -> String {
    style(s).cyan().to_string()
}

pub fn ok(s: &str) -> String {
    style(s).green().to_string()
}

pub fn warn(s: &str) -> String {
    style(s).yellow().to_string()
}

pub fn err(s: &str) -> String {
    style(s).red().to_string()
}

pub fn dim(s: &str) -> String {
    style(s).dim().to_string()
}

pub fn bold(s: &str) -> String {
    style(s).bold().to_string()
}

pub fn label(s: &str) -> String {
    style(format!("{s}:")).dim().to_string()
}

/// 成功图标 + 文案。
pub fn done(s: &str) -> String {
    format!("{} {}", style("✓").green(), s)
}

/// 失败图标 + 文案。
pub fn failed(s: &str) -> String {
    format!("{} {}", style("✗").red(), s)
}
