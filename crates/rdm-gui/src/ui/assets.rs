//! 资产打包：自定义 SVG 图标 + gpui-component 内建图标。
//!
//! 运行期零外部文件（安装目录只需要 exe）：图标通过 `include_bytes`/`rust-embed`
//! 编进二进制。自定义图标路径统一为 `rdm-icons/<name>.svg`，其余路径委托给
//! `gpui_component_assets::Assets`（其标准图标路径为 `icons/<name>.svg`）。

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// 内置在 `crates/rdm-gui/assets/icons/*.svg` 的 RDM 图标（Lucide，ISC）。
#[derive(rust_embed::RustEmbed)]
#[folder = "assets/icons"]
#[include = "*.svg"]
pub struct RdmIconFiles;

/// 应用 AssetSource：同时提供 RDM 图标与 gpui-component 标准图标。
pub struct RdmAssets;

impl AssetSource for RdmAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        if let Some(name) = path.strip_prefix("rdm-icons/") {
            return Ok(RdmIconFiles::get(name).map(|file| file.data));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out: Vec<SharedString> = RdmIconFiles::iter()
            .filter_map(|name| {
                let full = format!("rdm-icons/{name}");
                full.starts_with(path).then(|| full.into())
            })
            .collect();
        out.extend(gpui_component_assets::Assets.list(path)?);
        Ok(out)
    }
}
