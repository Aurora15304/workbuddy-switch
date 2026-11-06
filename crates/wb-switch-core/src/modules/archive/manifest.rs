//! 归档清单（`archive-manifest.json`）：描述「这包是谁、什么时候、导出了哪些组」。
//!
//! 单独成文件而不是依赖 zip 文件名/目录结构推断：
//! - `inspect()` 只读它就能给出分组清单，不需要解压整包；
//! - 将来新增分组时，旧版本至少能如实告知「有哪些组我认不出」，
//!   而不是把不认识的条目当成损坏直接失败；
//! - `formatVersion` 是格式演进的唯一开关，字段增删都靠它判断兼容性。

use serde::{Deserialize, Serialize};

/// 当前写出的格式版本。读到更高版本直接拒收：宁可不导入，也不半懂地写盘。
pub const FORMAT_VERSION: u32 = 1;

/// 清单在 zip 内的固定路径（不在任何分组前缀下，避免被当成组数据）。
pub const MANIFEST_NAME: &str = "archive-manifest.json";

/// 清单内容。字段用 camelCase 与前端 TS 对齐（宿主可能直接把 manifest 透传出去）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format_version: u32,
    /// 导出时的应用版本（`update::APP_VERSION`），仅用于展示与排查。
    pub app_version: String,
    /// 导出时间，epoch 秒。
    pub exported_at: i64,
    pub groups: Vec<ManifestGroup>,
}

/// 清单里的单个分组统计：`files` / `bytes` 是导出那一刻的实际值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestGroup {
    pub id: String,
    pub files: u64,
    pub bytes: u64,
}

impl Manifest {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| format!("清单序列化失败：{e}"))
    }

    /// 解析清单并做最低限度的语义校验：版本与分组 id 必须可用。
    pub fn from_json(raw: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(raw)
            .map_err(|_| "归档清单不是 UTF-8 文本，归档可能已损坏".to_string())?;
        let manifest: Manifest = serde_json::from_str(text)
            .map_err(|e| format!("归档清单无法解析：{e}"))?;
        if manifest.format_version != FORMAT_VERSION {
            return Err(format!(
                "归档格式版本为 {}，当前版本只支持 {FORMAT_VERSION}",
                manifest.format_version
            ));
        }
        if manifest.groups.is_empty() {
            return Err("归档清单里没有任何分组".to_string());
        }
        if manifest.groups.iter().any(|g| g.id.trim().is_empty()) {
            return Err("归档清单里存在空分组 id".to_string());
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION,
            app_version: "9.9.9".to_string(),
            exported_at: 1_700_000_000,
            groups: vec![ManifestGroup {
                id: "accounts".to_string(),
                files: 1,
                bytes: 12,
            }],
        }
    }

    #[test]
    fn round_trip_uses_camel_case_keys() {
        let json = sample().to_json().unwrap();
        assert!(json.contains("\"formatVersion\""), "应输出 camelCase：{json}");
        assert!(json.contains("\"exportedAt\""));
        assert!(!json.contains("\"format_version\""));
        let parsed = Manifest::from_json(json.as_bytes()).unwrap();
        assert_eq!(parsed.format_version, FORMAT_VERSION);
        assert_eq!(parsed.groups.len(), 1);
        assert_eq!(parsed.groups[0].id, "accounts");
    }

    #[test]
    fn rejects_unsupported_version_and_empty_groups() {
        let mut newer = sample();
        newer.format_version = FORMAT_VERSION + 1;
        assert!(Manifest::from_json(newer.to_json().unwrap().as_bytes()).is_err());

        let mut empty = sample();
        empty.groups.clear();
        assert!(Manifest::from_json(empty.to_json().unwrap().as_bytes()).is_err());
    }

    #[test]
    fn rejects_non_utf8_and_garbage() {
        assert!(Manifest::from_json(&[0xff, 0xfe]).is_err());
        assert!(Manifest::from_json(b"not json").is_err());
    }
}
