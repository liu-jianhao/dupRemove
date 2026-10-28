//! store：SQLite 增量缓存。缓存键 = 路径 + mtime + size，命中则跳过指纹计算。

use crate::finger::{Fingerprint, COLOR_SIG_LEN};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        Store::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Store> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS fingerprints (
                path              TEXT PRIMARY KEY,
                size              INTEGER NOT NULL,
                mtime             INTEGER NOT NULL,
                phash             INTEGER,
                csig              BLOB,
                width             INTEGER,
                height            INTEGER,
                has_exif_camera   INTEGER NOT NULL DEFAULT 0,
                taken_at          TEXT,
                dev               INTEGER,
                ino               INTEGER,
                algo_version      INTEGER
            );",
        )?;
        // 迁移：老库只有 dhash 列。dhash 的值不能当 pHash 用（算法不同），
        // 所以另开 phash / csig 两列；老行两列都是 NULL → 缓存未命中 →
        // 自动重算一次，之后就恢复正常命中。
        ensure_column(&conn, "phash", "INTEGER")?;
        ensure_column(&conn, "csig", "BLOB")?;
        // dev/ino 是阶段 1 加的文件身份列（删除前校验用），老库补列后
        // 首次重算指纹时回填。mtime 列语义已从秒升级为纳秒：
        // 老行的秒级值与新的纳秒值天然不匹配 → 未命中 → 重算回写，无需显式迁移。
        ensure_column(&conn, "dev", "INTEGER")?;
        ensure_column(&conn, "ino", "INTEGER")?;
        // 指纹算法版本（docs/04 阶段 2）：NULL（老行）或不等于当前版本的行
        // 一律未命中重算 —— 改指纹算法时只需 bump finger::ALGO_VERSION。
        ensure_column(&conn, "algo_version", "INTEGER")?;
        Ok(Store { conn })
    }

    /// 缓存命中查询：path 存在且 size/mtime 未变 → 返回缓存指纹。
    ///
    /// `csig` 为 NULL 表示这行来自旧版本库、还没算过新指纹 → 视为未命中。
    /// `csig` 为空 BLOB 表示「试过解码但失败」（HEIC / 损坏文件）→ 算命中，
    /// 免得每次扫描都重新尝试解码。
    pub fn lookup(&self, path: &Path, size: u64, mtime_ns: u64) -> Option<Fingerprint> {
        let key = path.to_string_lossy().to_string();
        let row = self
            .conn
            .query_row(
                "SELECT path, size, mtime, phash, csig, width, height, has_exif_camera, taken_at, dev, ino
                 FROM fingerprints WHERE path = ?1 AND algo_version = ?2",
                params![key, crate::finger::ALGO_VERSION],
                |row| {
                    let csig_raw: Option<Vec<u8>> = row.get(4)?;
                    let computed = csig_raw.is_some();
                    let csig = csig_raw.and_then(|v| {
                        if v.len() == COLOR_SIG_LEN {
                            let mut a = [0u8; COLOR_SIG_LEN];
                            a.copy_from_slice(&v);
                            Some(a)
                        } else {
                            None
                        }
                    });
                    Ok((
                        Fingerprint {
                            path: Path::new(&row.get::<_, String>(0)?).to_path_buf(),
                            size: row.get(1)?,
                            mtime_ns: row.get(2)?,
                            dev: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
                            ino: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
                            phash: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                            csig,
                            width: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
                            height: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
                            has_exif_camera: row.get::<_, i64>(7)? != 0,
                            taken_at: row.get(8)?,
                        },
                        computed,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten();

        match row {
            Some((fp, true)) if fp.size == size && fp.mtime_ns == mtime_ns => Some(fp),
            _ => None,
        }
    }

    fn upsert_on(conn: &Connection, fp: &Fingerprint) -> rusqlite::Result<()> {
        // 空 BLOB = 解码失败（缓存命中但无指纹）；COLOR_SIG_LEN 字节 = 解码成功。
        // 长度对不上的旧签名（算法改版会改长度）同样落到"未命中"分支，自动重算。
        let csig: Vec<u8> = match &fp.csig {
            Some(c) => c.to_vec(),
            None => Vec::new(),
        };
        conn.execute(
            "INSERT INTO fingerprints (path, size, mtime, phash, csig, width, height, has_exif_camera, taken_at, dev, ino, algo_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(path) DO UPDATE SET
                size = excluded.size,
                mtime = excluded.mtime,
                phash = excluded.phash,
                csig = excluded.csig,
                width = excluded.width,
                height = excluded.height,
                has_exif_camera = excluded.has_exif_camera,
                taken_at = excluded.taken_at,
                dev = excluded.dev,
                ino = excluded.ino,
                algo_version = excluded.algo_version",
            params![
                fp.path.to_string_lossy().to_string(),
                fp.size as i64,
                fp.mtime_ns as i64,
                fp.phash.map(|h| h as i64),
                csig,
                fp.width.map(|v| v as i64),
                fp.height.map(|v| v as i64),
                fp.has_exif_camera as i64,
                fp.taken_at,
                fp.dev.map(|v| v as i64),
                fp.ino.map(|v| v as i64),
                crate::finger::ALGO_VERSION,
            ],
        )?;
        Ok(())
    }

    pub fn upsert(&self, fp: &Fingerprint) -> rusqlite::Result<()> {
        Self::upsert_on(&self.conn, fp)
    }

    /// 单事务批量写入，避免每条 INSERT 都独立提交、触发一次 fsync。
    pub fn upsert_all(&self, fps: &[Fingerprint]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for fp in fps {
            Self::upsert_on(&tx, fp)?;
        }
        tx.commit()
    }

    /// 只清理本次扫描目录下已不存在的文件；批量删除共用一个事务。
    /// SQL 先按目录缩小候选集，避免缓存增长后每次扫描都全表 exists()。
    pub fn prune_missing(&self, root: &Path) -> rusqlite::Result<usize> {
        let root_key = root.to_string_lossy().to_string();
        let mut prefix = root_key.clone();
        if !prefix.ends_with(std::path::MAIN_SEPARATOR) {
            prefix.push(std::path::MAIN_SEPARATOR);
        }
        // LIKE 通配符必须按普通路径字符处理；ESCAPE '\' 与下面三处转义对应。
        let like_prefix = format!(
            "{}%",
            prefix
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let candidates: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT path FROM fingerprints
                 WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            )?;
            let rows = stmt.query_map(params![root_key, like_prefix], |row| {
                row.get::<_, String>(0)
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let missing: Vec<String> = candidates
            .into_iter()
            .filter(|path| !Path::new(path).exists())
            .collect();

        let tx = self.conn.unchecked_transaction()?;
        let mut removed = 0;
        for path in missing {
            removed += tx.execute("DELETE FROM fingerprints WHERE path = ?1", params![path])?;
        }
        tx.commit()?;
        Ok(removed)
    }
}

/// 幂等地补一列（SQLite 的 CREATE TABLE IF NOT EXISTS 不会改已有表）。
fn ensure_column(conn: &Connection, name: &str, ty: &str) -> rusqlite::Result<()> {
    let existing: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(fingerprints)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if existing.iter().any(|c| c == name) {
        return Ok(());
    }
    conn.execute(
        &format!("ALTER TABLE fingerprints ADD COLUMN {name} {ty}"),
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 构造测试指纹。`csig` 给 None 表示「解码失败」，Some 给全 0 网格。
    fn fp(path: &str, size: u64, mtime_ns: u64, phash: Option<u64>) -> Fingerprint {
        Fingerprint {
            path: PathBuf::from(path),
            size,
            mtime_ns,
            dev: None,
            ino: None,
            phash,
            csig: phash.map(|_| [7u8; COLOR_SIG_LEN]),
            width: Some(100),
            height: Some(80),
            has_exif_camera: false,
            taken_at: None,
        }
    }

    #[test]
    fn cache_hit_and_miss() {
        let store = Store::open_in_memory().unwrap();
        store.upsert(&fp("/tmp/a.jpg", 10, 100, Some(42))).unwrap();

        // 命中：size/mtime 一致
        let hit = store.lookup(Path::new("/tmp/a.jpg"), 10, 100);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().phash, Some(42));

        // 未命中：mtime 变了
        assert!(store.lookup(Path::new("/tmp/a.jpg"), 10, 200).is_none());
        // 未命中：size 变了
        assert!(store.lookup(Path::new("/tmp/a.jpg"), 11, 100).is_none());
        // 未命中：新文件
        assert!(store.lookup(Path::new("/tmp/b.jpg"), 10, 100).is_none());
    }

    #[test]
    fn upsert_overwrites() {
        let store = Store::open_in_memory().unwrap();
        store.upsert(&fp("/tmp/a.jpg", 10, 100, None)).unwrap();
        store.upsert(&fp("/tmp/a.jpg", 10, 100, Some(7))).unwrap();
        let hit = store.lookup(Path::new("/tmp/a.jpg"), 10, 100).unwrap();
        assert_eq!(hit.phash, Some(7));
    }

    #[test]
    fn file_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cache.db");
        {
            let store = Store::open(&db).unwrap();
            store.upsert(&fp("/tmp/a.jpg", 10, 100, Some(1))).unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert!(store.lookup(Path::new("/tmp/a.jpg"), 10, 100).is_some());
    }

    #[test]
    fn prune_missing_files_only_under_scan_root() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.jpg");
        let missing = dir.path().join("missing.jpg");
        let outside = PathBuf::from("/nonexistent/outside.jpg");
        std::fs::write(&real, b"x").unwrap();

        let store = Store::open_in_memory().unwrap();
        store
            .upsert(&fp(real.to_str().unwrap(), 10, 100, Some(1)))
            .unwrap();
        store
            .upsert(&fp(missing.to_str().unwrap(), 10, 100, Some(2)))
            .unwrap();
        store
            .upsert(&fp(outside.to_str().unwrap(), 10, 100, Some(3)))
            .unwrap();

        let removed = store.prune_missing(dir.path()).unwrap();
        assert_eq!(removed, 1);
        assert!(store.lookup(&real, 10, 100).is_some());
        assert!(store.lookup(&missing, 10, 100).is_none());
        // 其他目录的缓存不属于本次扫描，不能顺手清理。
        assert!(store.lookup(&outside, 10, 100).is_some());
    }

    #[test]
    fn upsert_all_persists_every_fingerprint() {
        let store = Store::open_in_memory().unwrap();
        let fps = vec![
            fp("/tmp/a.jpg", 10, 100, Some(1)),
            fp("/tmp/b.jpg", 20, 200, Some(2)),
            fp("/tmp/c.jpg", 30, 300, Some(3)),
        ];
        store.upsert_all(&fps).unwrap();
        for expected in fps {
            let hit = store
                .lookup(&expected.path, expected.size, expected.mtime_ns)
                .unwrap();
            assert_eq!(hit.phash, expected.phash);
            assert_eq!(hit.csig, expected.csig);
        }
    }

    #[test]
    fn color_signature_survives_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let mut f = fp("/tmp/c.jpg", 10, 100, Some(9));
        let mut sig = [0u8; COLOR_SIG_LEN];
        for (i, v) in sig.iter_mut().enumerate() {
            *v = (i * 5) as u8;
        }
        f.csig = Some(sig);
        store.upsert(&f).unwrap();
        let hit = store.lookup(Path::new("/tmp/c.jpg"), 10, 100).unwrap();
        assert_eq!(hit.csig, Some(sig));
    }

    #[test]
    fn undecodable_file_is_cached_as_failure() {
        // HEIC / 损坏文件解不出指纹：不该每次扫描都重试解码。
        let store = Store::open_in_memory().unwrap();
        store.upsert(&fp("/tmp/bad.heic", 10, 100, None)).unwrap();
        let hit = store.lookup(Path::new("/tmp/bad.heic"), 10, 100);
        let hit = hit.expect("解码失败的文件也应缓存命中");
        assert!(hit.phash.is_none());
        assert!(hit.csig.is_none());
    }

    #[test]
    fn legacy_row_without_color_sig_is_a_cache_miss() {
        // 模拟旧版本库：只有 dhash 列，phash/csig 都是 NULL。
        // 命中不了 → 自动重算一次，之后恢复正常。
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE fingerprints (
                    path              TEXT PRIMARY KEY,
                    size              INTEGER NOT NULL,
                    mtime             INTEGER NOT NULL,
                    dhash             INTEGER,
                    width             INTEGER,
                    height            INTEGER,
                    has_exif_camera   INTEGER NOT NULL DEFAULT 0,
                    taken_at          TEXT
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO fingerprints (path, size, mtime, dhash) VALUES ('/tmp/old.jpg', 10, 100, 12345)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert!(
            store.lookup(Path::new("/tmp/old.jpg"), 10, 100).is_none(),
            "旧库的行必须视为未命中，让它重算一次"
        );
    }
}
