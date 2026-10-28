//! dupRemove — 完全离线的照片去重工具。
//! 三原则：绝不联网 / 绝不硬删除 / 先预览后执行。

pub mod cluster;
pub mod commands;
pub mod control;
pub mod decode;
pub mod finger;
pub mod keeper;
pub mod pipeline;
pub mod scanner;
pub mod session;
pub mod store;
pub mod thumbs;
pub mod trashbin;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Mutex;
use tauri::Manager;

/// 准备缩略图目录，并按最小权限放行 asset 协议。
///
/// 返回 canonical 化后的目录：asset scope 用 glob 模式匹配（不解析软链），
/// 而被请求的路径在匹配前会被 canonicalize —— 两边必须同形，
/// 否则 /var → /private/var 这类软链会让缩略图 404。
fn prepare_thumb_dir<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    data_dir: &Path,
) -> tauri::Result<PathBuf> {
    let thumb_dir = data_dir.join("thumbs");
    std::fs::create_dir_all(&thumb_dir)?;
    let thumb_dir = std::fs::canonicalize(&thumb_dir).unwrap_or(thumb_dir);
    // asset 协议最小权限：配置里 scope 为空（默认拒绝一切），
    // 这里只放行缩略图缓存目录 —— 前端仅用它渲染预览图，
    // 无需也不应能通过 asset:// 读取用户照片目录或系统文件。
    app.asset_protocol_scope()
        .allow_directory(&thumb_dir, true)?;
    Ok(thumb_dir)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let thumb_dir = prepare_thumb_dir(app.handle(), &data_dir)?;
            app.manage(commands::AppState {
                fps: Mutex::new(Vec::new()),
                stashed_fps: Mutex::new(std::collections::HashMap::new()),
                undo_batches: Mutex::new(std::collections::HashMap::new()),
                next_batch_id: AtomicU64::new(0),
                active_scan: Mutex::new(None),
                next_scan_id: AtomicU64::new(0),
                thumb_dir,
                db_path: data_dir.join("cache.db"),
                journal_dir: data_dir.join("trash_batches"),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_scan,
            commands::cancel_scan,
            commands::regroup,
            commands::trash_files,
            commands::restore_files
        ])
        .run(tauri::generate_context!())
        .expect("error while running dupremove application");
}

#[cfg(test)]
mod tests {
    use crate::prepare_thumb_dir;
    use tauri::utils::config::{Csp, CspDirectiveSources, FsScope};
    use tauri::Manager;

    /// 把 CSP 配置摊平成可断言的字符串（兼容 Policy / DirectiveMap 两种写法）
    fn csp_text(csp: &Csp) -> String {
        match csp {
            Csp::Policy(p) => p.clone(),
            Csp::DirectiveMap(map) => map
                .iter()
                .map(|(k, v)| {
                    let sources = match v {
                        CspDirectiveSources::Inline(s) => s.clone(),
                        CspDirectiveSources::List(l) => l.join(" "),
                    };
                    format!("{k} {sources}")
                })
                .collect::<Vec<_>>()
                .join("; "),
        }
    }

    /// 安全配置回归测试。
    ///
    /// 防止有人把 csp 改回 null，或把 asset 协议的 scope 放开成全量访问
    /// （`scope: ["**"]` 等于允许前端读取本机任意文件）。
    #[test]
    fn security_config_stays_locked_down() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let raw = std::fs::read_to_string(&path).expect("读取 tauri.conf.json 失败");
        let cfg: tauri::utils::config::Config =
            serde_json::from_str(&raw).expect("tauri.conf.json 不符合 Tauri schema");
        let sec = &cfg.app.security;

        let text = csp_text(sec.csp.as_ref().expect("csp 不得为 null"));
        assert!(
            text.contains("default-src 'self'"),
            "生产 CSP 缺少 default-src 'self': {text}"
        );
        assert!(
            text.contains("object-src 'none'"),
            "生产 CSP 缺少 object-src 'none': {text}"
        );
        assert!(
            !text.contains("unsafe-eval"),
            "生产 CSP 不允许 unsafe-eval: {text}"
        );
        assert!(
            text.contains("ipc:"),
            "生产 CSP 缺少 IPC 通道 ipc:，invoke 会被浏览器拦下: {text}"
        );
        assert!(
            text.contains("asset:"),
            "生产 CSP 缺少 asset:，缩略图无法加载: {text}"
        );

        let dev_text = csp_text(
            sec.dev_csp
                .as_ref()
                .expect("devCsp 不得为 null：缺省时 dev 会套用生产 CSP，HMR 会被打断"),
        );
        assert!(
            dev_text.contains("ws://localhost:1420") && dev_text.contains("ws://localhost:1421"),
            "dev CSP 缺少 HMR websocket（1420 为桌面端默认，1421 为 TAURI_DEV_HOST 模式）: {dev_text}"
        );

        // asset 协议：配置层必须拒绝一切，运行时（lib.rs setup）只放行缩略图目录
        let allowed: &[std::path::PathBuf] = match &sec.asset_protocol.scope {
            FsScope::AllowedPaths(v) => v,
            FsScope::Scope { allow, .. } => allow,
        };
        assert!(
            allowed.is_empty(),
            "asset scope 不应在配置层放行路径（只在运行时放行缩略图目录）: {allowed:?}"
        );
    }

    /// asset 协议最小权限的行为验证。
    ///
    /// 用 Tauri 的 mock runtime 构造真实 scope：配置层默认拒绝一切，
    /// 运行时只放行缩略图目录 —— 目录内的缩略图可读，目录外一律拒绝。
    #[test]
    fn asset_scope_only_allows_thumb_dir() {
        use tauri::test::mock_app;

        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("创建数据目录失败");

        let app = mock_app();
        let scope = app.asset_protocol_scope();

        // 授权前：默认拒绝一切
        assert!(!scope.is_allowed("/etc/passwd"), "未授权时不应放行任何路径");

        let thumb_dir = prepare_thumb_dir(app.handle(), &data_dir).expect("准备缩略图目录失败");
        let thumb_file = thumb_dir.join("0123456789abcdef.jpg");
        std::fs::write(&thumb_file, b"jpeg").expect("写入缩略图失败");

        // 缩略图可读（前端靠 asset:// 渲染预览图）
        assert!(
            scope.is_allowed(&thumb_file),
            "缩略图应被放行: {}",
            thumb_file.display()
        );

        // 目录之外读不到 —— 这正是 scope 从 ["**"] 收窄后换来的收益
        let secret = data_dir.join("secret.txt");
        std::fs::write(&secret, b"x").expect("写入文件失败");
        assert!(
            !scope.is_allowed(&secret),
            "缩略图目录之外的文件不应被放行: {}",
            secret.display()
        );
        assert!(!scope.is_allowed("/etc/passwd"), "系统文件不应被放行");
    }
}
