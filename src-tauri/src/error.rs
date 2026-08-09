//! 统一错误类型。
//!
//! `code()` 返回稳定的机器可读标识，会写进 `items.error_code` 并被前端用来
//! 决定提示文案——所以**改动既有 code 字符串等同于破坏数据库里的历史记录**。

use serde::Serialize;
use ts_rs::TS;

pub type Result<T> = std::result::Result<T, ZzError>;

#[derive(Debug, thiserror::Error)]
pub enum ZzError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("数据库错误: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("找不到 {0}——请确认 ffmpeg sidecar 已随应用打包，或已安装在 PATH 中")]
    ToolNotFound(&'static str),

    #[error("{tool} 退出码 {code}: {stderr}")]
    ToolFailed { tool: &'static str, code: i32, stderr: String },

    #[error("配置无效: {0}")]
    BadConfig(String),

    /// 开跑前的空间预检没过（§8）。
    ///
    /// 单独一个变体而不是塞进 `Other`：这是唯一一个「用户改一下就能继续」的
    /// 启动失败，前端要能认出来并给出下一步（换个输出盘 / 腾空间），
    /// 而不是弹一句红字了事。
    #[error("{target} 空间不足：预计需要 {required}，当前可用 {available}")]
    NotEnoughSpace { target: String, required: String, available: String },

    /// 跨线程复制过的错误。
    ///
    /// `ZzError` 内含 `io::Error`，不是 `Clone`；而一个结果常常要分两份用
    /// （一份进事件回调、一份进汇总）。退化成字符串是可以的，**但 code 必须
    /// 跟着走**——否则异常列表里所有失败都会显示成 `other`，用户看不出是缺工具
    /// 还是盘满了。见 [`crate::core::orchestrator`] 里的 `clone_result`。
    #[error("{message}")]
    Cloned { code: &'static str, message: String },

    #[error("{0}")]
    Other(String),
}

impl ZzError {
    /// 稳定的错误码，写库与前端分支都靠它。
    pub fn code(&self) -> &'static str {
        match self {
            ZzError::Io(_) => "io",
            ZzError::Db(_) => "db",
            ZzError::Json(_) => "json",
            ZzError::ToolNotFound(_) => "tool_not_found",
            ZzError::ToolFailed { .. } => "tool_failed",
            ZzError::BadConfig(_) => "bad_config",
            ZzError::NotEnoughSpace { .. } => "no_space",
            ZzError::Cloned { code, .. } => code,
            ZzError::Other(_) => "other",
        }
    }

    /// 复制一份，保住 code。见 [`ZzError::Cloned`]。
    pub fn cloned(&self) -> Self {
        ZzError::Cloned { code: self.code(), message: self.to_string() }
    }
}

/// 跨 IPC 边界传给前端的形态。
///
/// 不直接给 `ZzError` 实现 `Serialize`：错误里可能带绝对路径和 stderr 原文，
/// 经过这一层可以明确控制暴露什么。
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct IpcError {
    pub code: String,
    pub message: String,
}

impl From<ZzError> for IpcError {
    fn from(e: ZzError) -> Self {
        IpcError { code: e.code().to_string(), message: e.to_string() }
    }
}

impl serde::Serialize for ZzError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        IpcError::from_ref(self).serialize(s)
    }
}

impl IpcError {
    fn from_ref(e: &ZzError) -> Self {
        IpcError { code: e.code().to_string(), message: e.to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_unique() {
        // 这些字符串会落库，改动即破坏历史记录的可读性。
        let samples = [
            (ZzError::ToolNotFound("ffmpeg"), "tool_not_found"),
            (ZzError::BadConfig("x".into()), "bad_config"),
            (ZzError::Other("x".into()), "other"),
            (ZzError::ToolFailed { tool: "ffmpeg", code: 1, stderr: String::new() }, "tool_failed"),
        ];
        for (e, expected) in samples {
            assert_eq!(e.code(), expected);
        }
    }

    #[test]
    fn a_cloned_error_keeps_its_original_code() {
        // 复制丢掉 code 的话，异常列表里所有失败都会挤成同一类。
        let e = ZzError::ToolFailed { tool: "ffmpeg", code: 1, stderr: "盘满".into() };
        let c = e.cloned();
        assert_eq!(c.code(), "tool_failed");
        assert_eq!(c.to_string(), e.to_string());
        assert_eq!(c.cloned().code(), "tool_failed", "复制两次也不能退化");
    }

    #[test]
    fn serializes_to_code_and_message() {
        let json = serde_json::to_value(ZzError::ToolNotFound("ffprobe")).unwrap();
        assert_eq!(json["code"], "tool_not_found");
        assert!(json["message"].as_str().unwrap().contains("ffprobe"));
    }
}
