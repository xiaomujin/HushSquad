//! 客户端本地状态：设备号、信任的证书指纹、服务器下发的免密 token。
//!
//! 库文件放哪儿这件事值得明说：默认是**用户数据目录**而不是当前目录。
//! 从别的目录启动客户端时必须还是同一个设备号 —— 否则「凭设备号进」会在换个目录之后
//! 变成另一个人，而用户完全看不出发生了什么。

use std::path::PathBuf;

use store::client::{ClientStore, format_device_id};

/// 默认库路径。
///
/// - Windows：`%APPDATA%\HushSquad\client.db`
/// - 其它：`$XDG_DATA_HOME/hushsquad/client.db`，没有就用 `$HOME/.local/share/...`
/// - 都取不到（罕见）：当前目录的 `hushsquad-client.db`
#[must_use]
pub fn default_path() -> PathBuf {
    if let Ok(dir) = std::env::var("APPDATA") {
        return PathBuf::from(dir).join("HushSquad").join("client.db");
    }
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("hushsquad").join("client.db");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("hushsquad")
            .join("client.db");
    }
    PathBuf::from("hushsquad-client.db")
}

/// 打开本地库（路径为 `None` 时用 [`default_path`]）。
pub fn open(path: Option<&std::path::Path>) -> Result<ClientStore, String> {
    let path = path.map_or_else(default_path, std::path::Path::to_path_buf);
    ClientStore::open(&path).map_err(|e| format!("打开本地库 {} 失败：{e}", path.display()))
}

/// 拿到本机设备号：命令行指定 > 库里存的 > 新生成一个。
///
/// 命令行指定时会**写回**库里，这样下次不带 `--device-id` 也是同一个身份。
pub fn resolve_device_id(
    store: &ClientStore,
    explicit: Option<[u8; 16]>,
) -> Result<[u8; 16], String> {
    if let Some(id) = explicit {
        store
            .set_device_id(&id)
            .map_err(|e| format!("写设备号失败：{e}"))?;
        return Ok(id);
    }
    store
        .device_id_or_create()
        .map_err(|e| format!("取设备号失败：{e}"))
}

/// 打印设备号的两种形式（十六进制给人核对，短码给人念）。
#[must_use]
pub fn describe_device_id(id: &[u8; 16]) -> String {
    format!("设备号：{}", format_device_id(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_path_lands_in_a_user_directory() {
        let path = default_path();
        assert!(
            path.file_name().is_some_and(|name| name == "client.db"),
            "文件名应当固定，实际 {} ",
            path.display()
        );
        assert!(path.is_absolute() || path.components().count() == 1);
    }

    #[test]
    fn an_explicit_device_id_is_written_back() {
        let store = ClientStore::open_memory().expect("内存库");
        let id = [0xAB; 16];
        assert_eq!(resolve_device_id(&store, Some(id)).unwrap(), id);
        assert_eq!(
            store.device_id().unwrap(),
            Some(id),
            "显式给过的设备号必须记住，下次不带参数也是同一个人"
        );
    }

    #[test]
    fn the_device_id_is_stable_across_calls() {
        let store = ClientStore::open_memory().expect("内存库");
        let first = resolve_device_id(&store, None).unwrap();
        let second = resolve_device_id(&store, None).unwrap();
        assert_eq!(first, second);
        assert!(describe_device_id(&first).starts_with("设备号："));
    }
}
